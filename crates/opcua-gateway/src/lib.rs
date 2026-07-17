//! The OPC UA server — the face of the product. Address space is generated
//! from the resolved config (one folder per device, one Variable per tag);
//! live values flow in from the typed store's change stream.
//!
//! Read path (this phase): `TypedBatch` -> `DataValue { Variant, StatusCode,
//! SourceTimestamp }` -> `SimpleNodeManager::set_values` -> subscriptions.
//! Write path (OPC UA write -> inverse formula -> Modbus) is phase 4.

pub mod auth;
pub mod write;

use std::collections::HashMap;
use std::sync::Arc;

use opcua::server::address_space::Variable;
use opcua::server::diagnostics::NamespaceMetadata;
use opcua::server::node_manager::memory::{simple_node_manager, SimpleNodeManager};
use opcua::server::{ServerBuilder, ServerHandle};
use opcua::types::{BuildInfo, DataValue, DateTime, NodeId, StatusCode, Variant};

use gateway_config::schema::v1::OpcUaConfig;
use gateway_config::ResolvedConfig;
use mb_poller::{Quality, WriteCommand};
use mb_types::{ChannelId, TagId};
use tags_core::{TypedReader, TypedSnapshot, TypedValue};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub use auth::{hash_password, AuthEvent, GatewayAuthenticator};
pub use write::{build_write_plans, WritePlan, WritePlanError};

pub const NAMESPACE_URI: &str = "urn:opc-modbus-server:tags";

#[derive(Debug, thiserror::Error)]
pub enum OpcUaError {
    #[error("server build: {0}")]
    Build(String),
    #[error("opcua section disabled in config")]
    Disabled,
    #[error(transparent)]
    WritePlan(#[from] WritePlanError),
}

/// Handle to the running OPC UA server + its value pump.
pub struct OpcUaHandle {
    server_handle: ServerHandle,
    authenticator: Arc<GatewayAuthenticator>,
    pump_shutdown: watch::Sender<bool>,
    server_task: JoinHandle<Result<(), String>>,
    pump_task: JoinHandle<()>,
}

impl OpcUaHandle {
    /// Endpoint URL clients connect to (built from the ADVERTISED host, see
    /// [`OpcUaConfig::advertised_host`]).
    pub fn endpoint_url(&self) -> String {
        // ServerHandle knows the configured base endpoint via its info.
        self.server_handle.info().base_endpoint()
    }

    /// Number of LIVE OPC UA sessions, from the server's diagnostics
    /// (`CurrentSessionCount`): incremented on CreateSession, decremented on
    /// CloseSession and session expiry.
    pub fn session_count(&self) -> usize {
        use opcua::types::VariableId;
        self.server_handle
            .info()
            .diagnostics
            .get(VariableId::Server_ServerDiagnostics_ServerDiagnosticsSummary_CurrentSessionCount)
            .and_then(|dv| dv.value)
            .and_then(|v| match v {
                Variant::UInt32(n) => Some(n as usize),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Recent successful logins (user, endpoint, security, timestamp),
    /// oldest first, capped at 50.
    ///
    /// Named honestly: async-opcua 0.18 keeps its live session registry
    /// crate-private (`SessionManager`'s map has no public iteration — only
    /// `find_by_token`), so per-session details (client application
    /// description, remote address, connected-since) cannot be enumerated.
    /// What IS available live is [`Self::session_count`]; this history of
    /// authentication EVENTS complements it. See [`AuthEvent`].
    pub fn recent_authentications(&self) -> Vec<AuthEvent> {
        self.authenticator.recent_authentications()
    }

    /// Stop the pump and the server, wait for both.
    pub async fn shutdown(self) {
        let _ = self.pump_shutdown.send(true);
        self.server_handle.cancel();
        let _ = self.pump_task.await;
        let _ = self.server_task.await;
    }
}

/// Build and start the OPC UA server over the typed store.
///
/// The address space mirrors the config: `Objects/<device>/<tag>` with
/// string NodeIds `ns=<x>;s=<tag name>` (stable across restarts as long as
/// the tag name survives config edits — the design's NodeId-stability rule).
///
/// `writers` (from [`mb_poller::PollerHandle::all_writers`]) enables the
/// write path: writable tags get WRITE access and a callback that performs
/// the Modbus write-back. Pass an empty map for a read-only server.
pub fn spawn(
    cfg: &ResolvedConfig,
    typed: Arc<dyn TypedReader>,
    writers: HashMap<ChannelId, mpsc::Sender<WriteCommand>>,
) -> Result<OpcUaHandle, OpcUaError> {
    let opcua_cfg = &cfg.opcua;
    if !opcua_cfg.enabled {
        return Err(OpcUaError::Disabled);
    }

    // Compile write plans first: a bad write_formula fails the boot.
    let write_plans = build_write_plans(cfg, &writers)?;

    let (server, handle, authenticator) = build_server(opcua_cfg)?;

    let node_manager = handle
        .node_managers()
        .get_of_type::<SimpleNodeManager>()
        .ok_or_else(|| OpcUaError::Build("simple node manager missing".into()))?;
    let ns = handle
        .get_namespace_index(NAMESPACE_URI)
        .ok_or_else(|| OpcUaError::Build("tag namespace not registered".into()))?;

    build_address_space(cfg, ns, &node_manager);

    // Wire the write-back callbacks (phase 4).
    for (name, plan) in write_plans {
        let node = NodeId::new(ns, name.as_str());
        let plan = Arc::new(plan);
        node_manager.inner().add_write_callback(node, move |dv, _range| {
            let status = plan.execute(dv);
            if status.is_good() {
                tracing::info!(tag = %name, "OPC UA write acknowledged by device");
            } else {
                tracing::warn!(tag = %name, %status, "OPC UA write failed");
            }
            status
        });
    }

    // Initial values (retentive restores may already be present).
    let subscriptions = handle.subscriptions().clone();
    push_values(
        &node_manager,
        &subscriptions,
        ns,
        &*typed,
        typed.snapshot_all().iter().map(|(t, s)| (*t, s.clone())),
    );

    // ---- value pump ----
    let (sd_tx, mut sd_rx) = watch::channel(false);
    let pump_typed = Arc::clone(&typed);
    let pump_manager = Arc::clone(&node_manager);
    let pump_task = tokio::spawn(async move {
        let mut rx = pump_typed.subscribe();
        loop {
            tokio::select! {
                biased;
                res = sd_rx.changed() => {
                    if res.is_err() || *sd_rx.borrow() {
                        return;
                    }
                }
                msg = rx.recv() => match msg {
                    Ok(batch) => {
                        let snaps = batch.tags.iter().filter_map(|t| {
                            pump_typed.snapshot(*t).map(|s| (*t, s))
                        });
                        push_values(&pump_manager, &subscriptions, ns, &*pump_typed, snaps);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "typed stream lagged: full OPC UA refresh");
                        let all = pump_typed.snapshot_all();
                        push_values(&pump_manager, &subscriptions, ns, &*pump_typed, all.into_iter());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    });

    // B2: BIND on `host`; the ServerConfig built above carries the ADVERTISED
    // host, which `Server::run_with` uses only for endpoint/discovery URLs,
    // never for the socket (that is what plain `run()` would have done).
    let bind_addr = format!("{}:{}", opcua_cfg.host, opcua_cfg.port);
    let server_task = tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(bind_addr.as_str())
            .await
            .map_err(|e| format!("OPC UA bind on {bind_addr}: {e}"))?;
        server.run_with(listener).await.map_err(|e| e.to_string())
    });

    tracing::info!(
        host = %opcua_cfg.host,
        advertised_host = %opcua_cfg.advertised_host.as_deref().unwrap_or(&opcua_cfg.host),
        port = opcua_cfg.port,
        "OPC UA server started"
    );

    Ok(OpcUaHandle {
        server_handle: handle,
        authenticator,
        pump_shutdown: sd_tx,
        server_task,
        pump_task,
    })
}

fn build_server(
    c: &OpcUaConfig,
) -> Result<(opcua::server::Server, ServerHandle, Arc<GatewayAuthenticator>), OpcUaError> {
    let (builder, authenticator) = server_builder(c);
    let (server, handle) = builder.build().map_err(OpcUaError::Build)?;
    Ok((server, handle, authenticator))
}

/// Configure the [`ServerBuilder`] from our config section. Split from
/// [`build_server`] so tests can inspect the resulting `ServerConfig`
/// (advertised URLs) without building a full server.
///
/// B2: the builder's `host` — which async-opcua puts into the endpoint URLs
/// returned by GetEndpoints/CreateSession (`ServerInfo::base_endpoint`) and
/// which we also put into `discovery_urls` — is `advertised_host` when set,
/// falling back to the bind `host`. The SOCKET is bound separately in
/// [`spawn`] (via `Server::run_with`), always on `host`.
fn server_builder(c: &OpcUaConfig) -> (ServerBuilder, Arc<GatewayAuthenticator>) {
    use opcua::server::{ServerEndpoint, ServerUserToken};

    let mut user_ids: Vec<String> = Vec::new();
    if c.allow_anonymous {
        user_ids.push("ANONYMOUS".into());
    }
    for u in &c.users {
        user_ids.push(u.username.clone());
    }

    let advertised = c.advertised_host.as_deref().unwrap_or(&c.host);
    let authenticator = Arc::new(GatewayAuthenticator::from_config(c));

    let mut builder = ServerBuilder::new()
        .application_name(c.application_name.clone())
        .application_uri(c.application_uri.clone())
        .product_uri("urn:opc-modbus-server")
        .host(advertised)
        .port(c.port)
        .discovery_urls(vec![format!("opc.tcp://{}:{}/", advertised, c.port)])
        .create_sample_keypair(true)
        .pki_dir(c.pki_dir.clone())
        // Secure default (phase 5): unknown client certs are rejected into
        // pki/rejected/ for the operator to move to pki/trusted/. The
        // commissioning override is config-gated and warned about.
        .trust_client_certs(c.trust_any_client_cert)
        .with_authenticator(Arc::clone(&authenticator) as _)
        .diagnostics_enabled(true)
        .build_info(BuildInfo {
            product_uri: "urn:opc-modbus-server".into(),
            manufacturer_name: "OPC Modbus Server".into(),
            product_name: c.application_name.clone().into(),
            software_version: env!("CARGO_PKG_VERSION").into(),
            build_number: "0".into(),
            build_date: DateTime::now(),
        })
        .with_node_manager(simple_node_manager(
            NamespaceMetadata {
                namespace_uri: NAMESPACE_URI.to_owned(),
                ..Default::default()
            },
            "tags",
        ));

    for u in &c.users {
        // The token entry advertises the username/password policy on the
        // endpoints; actual credential checks happen in GatewayAuthenticator
        // (which also handles password_hash users), so no secret goes here.
        builder = builder.add_user_token(
            u.username.clone(),
            ServerUserToken::user_pass(u.username.clone(), String::new()),
        );
    }
    if c.allow_none_security {
        builder = builder.add_endpoint("none", ServerEndpoint::new_none("/", &user_ids));
    }
    if c.basic256sha256 {
        // Sign-only Basic256Sha256 is deprecated by the standard; offer
        // SignAndEncrypt plus the modern AES256-SHA256-RSA-PSS profile.
        builder = builder
            .add_endpoint(
                "basic256sha256_sign_encrypt",
                ServerEndpoint::new_basic256sha256_sign_encrypt("/", &user_ids),
            )
            .add_endpoint(
                "aes256_sha256_rsapss_sign_encrypt",
                ServerEndpoint::new_aes256_sha256_rsapss_sign_encrypt("/", &user_ids),
            );
    }

    (builder, authenticator)
}

/// `Objects/<device folder>/<tag Variable>` for every register in the config.
fn build_address_space(cfg: &ResolvedConfig, ns: u16, manager: &Arc<SimpleNodeManager>) {
    let address_space = manager.address_space();
    let mut space = address_space.write();

    for ch in &cfg.channels {
        for dev in &ch.devices {
            let folder_id = NodeId::new(ns, format!("dev:{}", dev.name));
            space.add_folder(&folder_id, &dev.name, &dev.name, &NodeId::objects_folder_id());

            // Initial value/status is set right after build by the full
            // snapshot push (Absent -> BadWaitingForInitialData).
            let vars: Vec<Variable> = dev
                .registers
                .iter()
                .map(|reg| {
                    let name = cfg
                        .tag_name(reg.tag)
                        .expect("resolved register has a name");
                    let mut v = Variable::new_data_value(
                        &NodeId::new(ns, name),
                        name,
                        name,
                        opcua_data_type(reg),
                        None,
                        None,
                        Variant::Empty,
                    );
                    if reg.writable {
                        use opcua::nodes::AccessLevel;
                        let lvl = AccessLevel::CURRENT_READ | AccessLevel::CURRENT_WRITE;
                        v.set_access_level(lvl);
                        v.set_user_access_level(lvl);
                    }
                    v
                })
                .collect();
            let _ = space.add_variables(vars, &folder_id);
        }
    }
}

/// Push a set of typed snapshots into the node manager (and thus into any
/// active subscriptions).
fn push_values(
    manager: &Arc<SimpleNodeManager>,
    subscriptions: &Arc<opcua::server::SubscriptionCache>,
    ns: u16,
    typed: &dyn TypedReader,
    snaps: impl Iterator<Item = (TagId, TypedSnapshot)>,
) {
    let updates: Vec<(NodeId, DataValue)> = snaps
        .filter_map(|(tag, snap)| {
            let name = typed.name(tag)?;
            Some((NodeId::new(ns, name), to_data_value(&snap)))
        })
        .collect();
    if updates.is_empty() {
        return;
    }
    if let Err(e) = manager.set_values(
        subscriptions,
        updates.iter().map(|(id, dv)| (id, None, dv.clone())),
    ) {
        tracing::warn!(error = %e, "set_values failed");
    }
}

/// The OPC UA DataType attribute for a tag, mirroring the engine's type rule:
/// formula / non-identity scale -> Double; otherwise the native decode type.
fn opcua_data_type(reg: &gateway_config::ResolvedRegister) -> opcua::types::DataTypeId {
    use mb_types::DataType as DT;
    use opcua::types::DataTypeId;

    let transformed = reg.formula.is_some() || reg.scale != 1.0 || reg.offset != 0.0;
    let is_boolish = matches!(reg.data_type, DT::Bit) || (reg.data_type == DT::Bitfield && reg.bit.is_some());
    if is_boolish {
        return DataTypeId::Boolean;
    }
    if transformed {
        return DataTypeId::Double;
    }
    match reg.data_type {
        DT::Bit => DataTypeId::Boolean,
        DT::Bitfield | DT::U16 | DT::U32 | DT::U64 | DT::Bcd => DataTypeId::UInt64,
        DT::I16 | DT::I32 | DT::I64 => DataTypeId::Int64,
        DT::F32 | DT::F64 => DataTypeId::Double,
        DT::Ascii => DataTypeId::String,
    }
}

fn to_data_value(snap: &TypedSnapshot) -> DataValue {
    let value = match &snap.value {
        TypedValue::Bool(b) => Variant::Boolean(*b),
        TypedValue::Int(v) => Variant::Int64(*v),
        TypedValue::UInt(v) => Variant::UInt64(*v),
        TypedValue::Float(v) => Variant::Double(*v),
        TypedValue::Text(s) => Variant::String(s.as_str().into()),
        TypedValue::Bytes(b) => Variant::ByteString(opcua::types::ByteString::from(b.as_slice())),
        TypedValue::Absent => Variant::Empty,
    };
    let status = match (matches!(snap.value, TypedValue::Absent), snap.quality) {
        (true, _) => StatusCode::BadWaitingForInitialData,
        (_, Quality::Good) => StatusCode::Good,
        (_, Quality::Uncertain) => StatusCode::UncertainLastUsableValue,
        (_, Quality::Bad) => StatusCode::BadCommunicationError,
    };
    DataValue {
        value: Some(value),
        status: Some(status),
        source_timestamp: Some(system_time_to_datetime(snap.source_ts)),
        ..Default::default()
    }
}

fn system_time_to_datetime(ts: std::time::SystemTime) -> DateTime {
    let utc: chrono::DateTime<chrono::Utc> = ts.into();
    DateTime::from(utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B2: with `advertised_host` set, the ServerConfig used for endpoint
    /// descriptions (`tcp_config.host` -> `base_endpoint()` -> GetEndpoints
    /// URLs) and the discovery URLs both carry the ADVERTISED host — the
    /// bind-all `host` never leaks into what clients see. The socket itself
    /// is bound on `host` in `spawn` (proven e2e in tests/opcua_advertised.rs).
    #[test]
    fn advertised_host_drives_endpoint_and_discovery_urls() {
        let cfg = OpcUaConfig {
            host: "0.0.0.0".into(),
            advertised_host: Some("192.168.0.2".into()),
            port: 4841,
            ..OpcUaConfig::default()
        };
        let (builder, _auth) = server_builder(&cfg);
        let sc = builder.config();
        assert_eq!(
            sc.discovery_urls,
            vec!["opc.tcp://192.168.0.2:4841/".to_string()]
        );
        assert_eq!(sc.tcp_config.host, "192.168.0.2");
    }

    /// Without `advertised_host` the bind host is advertised, as before.
    #[test]
    fn advertised_host_falls_back_to_bind_host() {
        let cfg = OpcUaConfig {
            host: "10.1.2.3".into(),
            advertised_host: None,
            port: 4840,
            ..OpcUaConfig::default()
        };
        let (builder, _auth) = server_builder(&cfg);
        let sc = builder.config();
        assert_eq!(sc.discovery_urls, vec!["opc.tcp://10.1.2.3:4840/".to_string()]);
        assert_eq!(sc.tcp_config.host, "10.1.2.3");
    }
}
