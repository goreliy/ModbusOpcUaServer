//! Built-in MCP management: one controller owns the runtime and configuration.
//! HTTP and stdio share the same tools. All hardware requests use the owning
//! channel queue when polling is active.
mod access;
mod client;
mod connection;
pub use client::Client;
pub use connection::{config_id, connection_info, default_config_path};
mod credentials;
mod discovery;
mod protocol;
mod sessions;
mod tools;
mod transport;
pub use sessions::McpClientView;

use crate::{stack::start_stack_detailed, RunningStack};
pub use access::check_access;
use gateway_config::{schema::v1::McpConfig, ResolvedConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tags_core::TypedStore;
use tokio::sync::{watch, Mutex};

pub const MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;
pub const OWNER_BUSY: &str = "MCP controller already owns this configuration";

#[derive(Clone)]
pub struct OpcUaView {
    pub sessions: usize,
    pub authentications: Vec<opcua_gateway::AuthEvent>,
}
impl OpcUaView {
    pub fn session_count(&self) -> usize {
        self.sessions
    }
    pub fn recent_authentications(&self) -> Vec<opcua_gateway::AuthEvent> {
        self.authentications.clone()
    }
}
#[derive(Clone, Default)]
pub struct ServerView {
    pub mcp_clients: Vec<McpClientView>,
    pub saved_revision: Option<String>,
    pub applied_revision: Option<String>,
    pub running: bool,
    pub revision: u64,
    pub typed: Option<Arc<TypedStore>>,
    pub metrics: Vec<(String, Arc<mb_poller::ChannelMetrics>)>,
    pub endpoint: Option<String>,
    pub opcua: Option<OpcUaView>,
    pub trial_remaining: Option<std::time::Duration>,
    pub error: Option<String>,
}

pub(super) struct State {
    desired: Value,
    seen: Vec<u8>,
    applied: Option<String>,
    active: Option<Value>,
    stack: Option<RunningStack>,
    last_error: Option<String>,
    audit: VecDeque<Value>,
}
pub struct Controller {
    pub path: PathBuf,
    pub settings: McpConfig,
    pub(super) state: Mutex<State>,
    jobs: parking_lot::Mutex<HashMap<String, Arc<discovery::ScanJob>>>,
    view: parking_lot::RwLock<ServerView>,
    pub(super) closed: AtomicBool,
    pub(super) exit: watch::Sender<bool>,
    tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    endpoint: parking_lot::RwLock<Option<transport::Endpoint>>,
    lease: parking_lot::Mutex<Option<std::fs::File>>,
    shutdown_guard: Mutex<()>,
    clients: parking_lot::Mutex<HashMap<String, sessions::Session>>,
}

pub fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub(super) fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
pub(super) fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(super) fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err("configuration exceeds 8 MiB".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("configuration exceeds 8 MiB".into());
    }
    Ok(bytes)
}
pub(super) fn parse_file(bytes: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| e.to_string())?
        .trim_start_matches('\u{feff}');
    serde_json::from_str(text).map_err(|e| e.to_string())
}
pub(super) fn validate(value: &Value, path: &Path) -> Result<ResolvedConfig, String> {
    let cfg = gateway_config::load_str_at(&value.to_string(), path).map_err(|e| e.to_string())?;
    let errors = tags_core::check_config_formulas(
        cfg.channels
            .iter()
            .flat_map(|c| &c.devices)
            .flat_map(|d| &d.registers)
            .map(|r| {
                (
                    cfg.tag_name(r.tag).unwrap(),
                    r.formula.as_deref(),
                    r.write_formula.as_deref(),
                )
            }),
    );
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(cfg)
}

impl Controller {
    pub async fn new(path: impl AsRef<Path>) -> Result<Arc<Self>, String> {
        let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".mcp-owner.lock");
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_path))
            .map_err(|e| format!("MCP owner lock: {e}"))?;
        fs2::FileExt::try_lock_exclusive(&lease).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(33) {
                OWNER_BUSY.to_owned()
            } else {
                format!("MCP owner lock: {e}")
            }
        })?;
        let seen = read_file(&path)?;
        let desired = parse_file(&seen)?;
        let settings: McpConfig =
            serde_json::from_value(desired.get("mcp").cloned().unwrap_or(json!({})))
                .map_err(|e| e.to_string())?;
        if !matches!(settings.host.parse::<std::net::IpAddr>(), Ok(ip) if ip.is_loopback()) {
            return Err("MCP host must be a loopback IP".into());
        }
        let (exit, _) = watch::channel(false);
        let controller = Arc::new(Self {
            path,
            settings,
            state: Mutex::new(State {
                desired,
                seen,
                applied: None,
                active: None,
                stack: None,
                last_error: None,
                audit: VecDeque::new(),
            }),
            jobs: parking_lot::Mutex::new(HashMap::new()),
            view: parking_lot::RwLock::new(ServerView::default()),
            closed: AtomicBool::new(false),
            exit,
            tasks: parking_lot::Mutex::new(Vec::new()),
            endpoint: parking_lot::RwLock::new(None),
            lease: parking_lot::Mutex::new(Some(lease)),
            shutdown_guard: Mutex::new(()),
            clients: parking_lot::Mutex::new(HashMap::new()),
        });
        transport::listen(&controller).await?;
        let _ = std::fs::remove_file(transport::stop_marker(&controller.path));
        let owner = Arc::clone(&controller);
        let mut exit = owner.exit.subscribe();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
            let mut pending = None;
            loop {
                tokio::select! {
                    biased;
                    _ = exit.changed() => break,
                    _ = tick.tick() => {
                        let mut state = owner.state.lock().await;
                        match read_file(&owner.path) {
                            Ok(bytes) if bytes != state.seen => {
                                if pending.as_ref() == Some(&bytes) {
                                    let result = parse_file(&bytes).and_then(|v| validate(&v,&owner.path).and_then(|cfg| {
                                        if cfg.mcp != owner.settings { return Err("MCP settings staged; restart the controller to apply them".into()); }
                                        if state.stack.is_some() && owner.jobs.lock().values().any(|j|j.offline && j.running()) { return Err("finish or cancel offline scans before changing the active runtime".into()); }
                                        Ok((v,cfg))
                                    }));
                                    match result {
                                        Ok((value,cfg)) => {
                                            state.desired = value;
                                            if let Some(stack) = state.stack.as_mut() {
                                                match stack.reload(cfg).await {
                                                    Ok(()) => { state.applied = Some(fingerprint(&bytes)); state.active = Some(state.desired.clone()); state.last_error = None; }
                                                    Err(e) => state.last_error = Some(e),
                                                }
                                            } else { state.last_error = None; }
                                        }
                                        Err(e) => state.last_error = Some(format!("external configuration rejected: {e}")),
                                    }
                                    state.seen = bytes; pending = None;
                                } else { pending = Some(bytes); }
                            }
                            Ok(_) => pending = None,
                            Err(e) => { pending = None; state.last_error = Some(format!("config read: {e}")); }
                        }
                        if state.stack.as_ref().is_some_and(|s|s.trial_expired()) {
                            if let Some(stack) = state.stack.take() { stack.shutdown().await; }
                            state.applied = None; state.active = None; state.last_error = Some("trial runtime limit reached".into());
                        }
                        owner.refresh_view(&state);
                    }
                }
            }
        });
        controller.tasks.lock().push(task);
        Ok(controller)
    }

    fn refresh_view(&self, state: &State) {
        let was_running = self.view.read().running;
        let mut view = ServerView {
            mcp_clients: self.client_views(),
            error: state.last_error.clone(),
            saved_revision: Some(fingerprint(&state.seen)),
            applied_revision: state.applied.clone(),
            ..Default::default()
        };
        if let Some(stack) = &state.stack {
            view.running = true;
            view.revision = stack.revision();
            view.typed = Some(stack.typed());
            view.metrics = stack.channel_metrics();
            view.endpoint = stack.endpoint_url();
            view.trial_remaining = stack.trial_remaining();
            view.opcua = stack.opcua().map(|o| OpcUaView {
                sessions: o.session_count(),
                authentications: o.recent_authentications(),
            });
        }
        let running = view.running;
        *self.view.write() = view;
        if running != was_running {
            let endpoint = self.endpoint.read().clone();
            if let Some(endpoint) = endpoint {
                if let Err(error) = transport::write_sidecar(self, &endpoint) {
                    tracing::error!(%error,"Cannot publish controller running state");
                }
            }
        }
    }
    pub fn view(&self) -> ServerView {
        self.view.read().clone()
    }
    pub fn url(&self) -> Option<String> {
        self.endpoint.read().as_ref().map(|e| e.url.clone())
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
    pub async fn start_server(&self) -> Result<(), String> {
        if self.is_closed() {
            return Err("controller is shutting down".into());
        }
        let mut state = self.state.lock().await;
        if self.is_closed() {
            return Err("controller is shutting down".into());
        }
        if state.stack.is_some() {
            return Ok(());
        }
        if self.jobs.lock().values().any(|j| j.offline && j.running()) {
            return Err("finish or cancel the offline scan before starting polling".into());
        }
        let prepared = async {
            let bytes = read_file(&self.path)?;
            let value = parse_file(&bytes)?;
            let cfg = validate(&value, &self.path)?;
            if cfg.mcp != self.settings {
                return Err("restart the controller to apply staged MCP settings".into());
            }
            let stack = start_stack_detailed(cfg)
                .await
                .map_err(|e| format!("runtime start failed: {e}"))?;
            Ok::<_, String>((bytes, value, stack))
        }
        .await;
        let (bytes, value, stack) = match prepared {
            Ok(result) => result,
            Err(error) => {
                state.last_error = Some(error.clone());
                self.refresh_view(&state);
                return Err(error);
            }
        };
        state.stack = Some(stack);
        state.active = Some(value.clone());
        state.desired = value;
        state.applied = Some(fingerprint(&bytes));
        state.seen = bytes;
        state.last_error = None;
        self.refresh_view(&state);
        Ok(())
    }
    pub async fn adopt(&self, mut stack: RunningStack) -> Result<(), String> {
        let mut state = self.state.lock().await;
        if state.stack.is_some() {
            stack.shutdown().await;
            return Err("controller already owns a runtime".into());
        }
        let prepared = async {
            let bytes = read_file(&self.path)?;
            let value = parse_file(&bytes)?;
            let cfg = validate(&value, &self.path)?;
            if cfg.mcp != self.settings {
                return Err("restart the controller to apply staged MCP settings".into());
            }
            stack.reload(cfg).await?;
            Ok::<_, String>((bytes, value))
        }
        .await;
        let (bytes, value) = match prepared {
            Ok(result) => result,
            Err(e) => {
                stack.shutdown().await;
                return Err(e);
            }
        };
        state.stack = Some(stack);
        state.applied = Some(fingerprint(&bytes));
        state.active = Some(value.clone());
        state.desired = value;
        state.seen = bytes;
        self.refresh_view(&state);
        Ok(())
    }
    pub async fn stop_server(&self) {
        let mut state = self.state.lock().await;
        if let Some(stack) = state.stack.take() {
            stack.shutdown().await;
        }
        state.applied = None;
        state.active = None;
        self.refresh_view(&state);
    }
    pub async fn reload_file(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let bytes = read_file(&self.path)?;
        let value = parse_file(&bytes)?;
        let cfg = validate(&value, &self.path)?;
        if cfg.mcp != self.settings {
            return Err("restart the controller to apply staged MCP settings".into());
        }
        if state.stack.is_some() && self.jobs.lock().values().any(|j| j.offline && j.running()) {
            return Err("finish or cancel offline scans before changing the active runtime".into());
        }
        if let Some(stack) = state.stack.as_mut() {
            stack.reload(cfg).await?;
            state.applied = Some(fingerprint(&bytes));
            state.active = Some(value.clone());
        }
        state.desired = value;
        state.seen = bytes;
        state.last_error = None;
        self.refresh_view(&state);
        Ok(())
    }
    pub async fn call(self: &Arc<Self>, name: &str, args: Value) -> Result<Value, String> {
        if self.is_closed() && name != "server_status" {
            return Err("controller is shutting down".into());
        }
        let started = Instant::now();
        let result = tools::call(self, name, args).await;
        let mut state = self.state.lock().await;
        state.audit.push_back(json!({"time_ms":now_ms(),"tool":name,"ok":result.is_ok(),"duration_ms":started.elapsed().as_millis()}));
        while state.audit.len() > 200 {
            state.audit.pop_front();
        }
        self.refresh_view(&state);
        result
    }
    pub async fn shutdown(&self) {
        let _guard = self.shutdown_guard.lock().await;
        self.closed.store(true, Ordering::Release);
        let _ = self.exit.send(true);
        // Finish registration of in-flight commands/jobs before collecting
        // handles. Their actor-locked recheck prevents new work after closure.
        drop(self.state.lock().await);
        let jobs: Vec<_> = self.jobs.lock().values().cloned().collect();
        for job in &jobs {
            job.cancel.store(true, Ordering::Release);
        }
        for job in &jobs {
            let task = job.task.lock().take();
            if let Some(task) = task {
                let _ = task.await;
            }
        }
        self.stop_server().await;
        let tasks = std::mem::take(&mut *self.tasks.lock());
        for mut task in tasks {
            if tokio::time::timeout(std::time::Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        transport::remove_endpoint(self);
        self.lease.lock().take();
    }
}

pub fn previous_runtime_was_running(path: &Path) -> bool {
    transport::previous_running(path).unwrap_or(false)
}
pub fn previous_runtime_intent(path: &Path) -> Option<bool> {
    transport::previous_running(path)
}
pub fn shutdown_was_requested(path: &Path) -> bool {
    std::fs::read(transport::stop_marker(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["requested"] == true)
}
pub async fn run_managed(
    path: PathBuf,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    tokio::pin!(shutdown);
    let mut client: Option<Client> = None;
    let mut attached_once = false;
    let mut running_intent = true;
    let mut previous_instance = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(current) = &client {
            tokio::select! {
                _ = &mut shutdown => {
                    current.call("server_stop",json!({"confirm":true})).await?;
                    return Ok(());
                }
                _ = tick.tick() => {}
            }
            if let Ok(status) = current.call("server_status", json!({})).await {
                running_intent = status["running"] == true;
                continue;
            }
            if let Some(intent) = previous_runtime_intent(&path) {
                running_intent = intent;
            }
            client = None;
        }
        if attached_once && shutdown_was_requested(&path) {
            return Ok(());
        }
        match Controller::new(&path).await {
            Ok(controller) => {
                if running_intent {
                    if let Err(error) = controller.start_server().await {
                        controller.shutdown().await;
                        return Err(error);
                    }
                }
                let mut exit = controller.exit.subscribe();
                tokio::select! {_ = &mut shutdown => {}, _ = exit.changed() => {}}
                controller.shutdown().await;
                return Ok(());
            }
            Err(error) if error == OWNER_BUSY => {
                match Client::wait(&path, std::time::Duration::from_millis(500)).await {
                    Ok(current) => {
                        let changed = previous_instance.as_deref() != Some(current.instance_id());
                        if running_intent && (!attached_once || changed) {
                            current.call("server_start", json!({})).await?;
                        }
                        previous_instance = Some(current.instance_id().to_owned());
                        attached_once = true;
                        client = Some(current);
                    }
                    Err(error) => {
                        if error.contains("os error 5") {
                            return Err(error);
                        }
                        tokio::select! {_ = &mut shutdown => return Ok(()), _ = tick.tick() => {}}
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
}
pub async fn run_controller(
    path: PathBuf,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    let controller = Controller::new(path).await?;
    let mut exit = controller.exit.subscribe();
    tokio::select! {_ = shutdown=>{},_ = exit.changed()=>{}}
    controller.shutdown().await;
    Ok(())
}
pub async fn run_stdio(path: PathBuf) -> Result<(), String> {
    transport::stdio(path).await
}

pub(super) fn redacted(mut value: Value) -> Value {
    if let Some(users) = value
        .pointer_mut("/opcua/users")
        .and_then(Value::as_array_mut)
    {
        for user in users {
            for key in ["password", "password_hash"] {
                if user.get(key).is_some() {
                    user[key] = json!("<redacted>");
                }
            }
        }
    }
    value
}
pub(super) fn value_json(value: &tags_core::TypedValue) -> Value {
    use tags_core::TypedValue::*;
    match value {
        Bool(v) => json!(v),
        Int(v) => json!(v),
        UInt(v) => json!(v),
        Float(v) => json!(v),
        Text(v) => json!(v),
        Bytes(v) => json!(v),
        Absent => Value::Null,
    }
}
pub(super) fn snapshot_json(name: &str, s: &tags_core::TypedSnapshot) -> Value {
    let kind = match &s.value {
        tags_core::TypedValue::Int(_) => "Int",
        tags_core::TypedValue::UInt(_) => "UInt",
        tags_core::TypedValue::Float(_) => "Float",
        tags_core::TypedValue::Bool(_) => "Bool",
        tags_core::TypedValue::Text(_) => "Text",
        tags_core::TypedValue::Bytes(_) => "Bytes",
        tags_core::TypedValue::Absent => "Absent",
    };
    json!({"tag":name,"value":value_json(&s.value),"value_type":kind,"quality":format!("{:?}",s.quality),"source_timestamp_ms":s.source_ts.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis(),"age_ms":s.mono.elapsed().as_millis(),"sequence":s.seq})
}
