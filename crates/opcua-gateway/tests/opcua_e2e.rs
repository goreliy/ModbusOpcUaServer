//! The product's end-to-end proof: a REAL OPC UA client reads a value that
//! originated in a (simulated) Modbus slave and passed through the poller,
//! the tag cache, the formula engine and the typed store.
//!
//! Modbus sim -> mb-poller -> TagCache -> TagEngine -> TypedStore -> OPC UA
//! server -> async-opcua CLIENT (session read).

use std::collections::HashMap;
use std::future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opcua::client::{ClientBuilder, IdentityToken};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    MessageSecurityMode, NodeId, ReadValueId, StatusCode, TimestampsToReturn, UserTokenPolicy,
    Variant,
};
use tokio::net::TcpListener;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::tcp::{accept_tcp_connection, Server},
    SlaveRequest,
};

type Banks = Arc<Mutex<HashMap<u8, HashMap<u16, u16>>>>;

#[derive(Clone)]
struct MultiSlave {
    banks: Banks,
}

impl tokio_modbus::server::Service for MultiSlave {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let SlaveRequest { slave, request } = req;
        let mut banks = self.banks.lock().unwrap();
        let Some(bank) = banks.get_mut(&slave) else {
            return future::ready(Err(ExceptionCode::GatewayTargetDevice));
        };
        let res = match request {
            Request::ReadHoldingRegisters(addr, cnt) => (0..cnt)
                .map(|i| bank.get(&(addr + i)).copied())
                .collect::<Option<Vec<u16>>>()
                .map(|regs| Some(Response::ReadHoldingRegisters(regs)))
                .ok_or(ExceptionCode::IllegalDataAddress),
            Request::WriteSingleRegister(addr, value) => {
                bank.insert(addr, value);
                Ok(Some(Response::WriteSingleRegister(addr, value)))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

async fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opcua_client_reads_modbus_values_through_the_whole_stack() {
    // ---- Modbus simulator: temp raw 237 (i16 x0.1 -> 23.7), speed 25 (*60 -> 1500) ----
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0u16, 237u16), (1, 25), (5, 100)]),
    )])));
    let mb_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mb_addr = mb_listener.local_addr().unwrap();
    let _mb_server = {
        let banks = Arc::clone(&banks);
        tokio::spawn(async move {
            let server = Server::new(mb_listener);
            let service = MultiSlave { banks };
            let on_connected = move |stream, socket_addr| {
                let service = service.clone();
                async move {
                    accept_tcp_connection(stream, socket_addr, move |_| Ok(Some(service.clone())))
                }
            };
            let _ = server.serve(&on_connected, |err| eprintln!("mb server error: {err}")).await;
        })
    };

    // ---- config: one TCP channel + OPC UA on a random free port ----
    let opc_port = free_port().await;
    let pki = tempfile::tempdir().unwrap();
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "opcua": {{
            "host": "127.0.0.1",
            "port": {opc_port},
            "pki_dir": {pki_dir:?}
        }},
        "poll_groups": [ {{ "id": "fast", "period_ms": 50 }} ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {mb_port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 250,
            "retry": {{ "max_retries": 0, "base_backoff_ms": 50, "max_backoff_ms": 200 }},
            "devices": [ {{ "id": "plc1", "unit_id": 1, "registers": [
                {{ "tag": "plc1.temp",  "poll_group": "fast", "function": "read_holding_registers",
                   "address": 0, "data_type": "i16", "scale": 0.1, "units": "degC" }},
                {{ "tag": "plc1.speed", "poll_group": "fast", "function": "read_holding_registers",
                   "address": 1, "data_type": "u16", "formula": "raw * 60" }},
                {{ "tag": "plc1.setpoint", "poll_group": "fast", "function": "read_holding_registers",
                   "address": 5, "data_type": "u16", "scale": 0.1, "writable": true }}
            ] }} ]
        }} ]
    }}"#,
        mb_port = mb_addr.port(),
        pki_dir = pki.path().join("pki"),
    ))
    .expect("config loads");

    // ---- the full product stack ----
    let (poller, cache) = mb_poller::Poller::spawn_with_cache(&cfg);
    let engine = tags_core::TagEngine::new(
        &cfg,
        Arc::clone(&cache) as Arc<dyn mb_poller::CacheReader>,
        None,
    )
    .unwrap();
    let (engine_handle, typed) = engine.spawn();
    let opcua = opcua_gateway::spawn(
        &cfg,
        typed as Arc<dyn tags_core::TypedReader>,
        poller.all_writers(),
    )
    .unwrap();

    // Give the server a moment to bind + generate its certificate.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ---- a real OPC UA client connects and reads ----
    let mut client = ClientBuilder::new()
        .application_name("e2e test client")
        .application_uri("urn:opc-modbus-server:e2e-client")
        .trust_server_certs(true)
        .create_sample_keypair(false)
        .pki_dir(pki.path().join("client-pki").to_string_lossy().to_string())
        .session_retry_limit(3)
        .client()
        .unwrap();

    let endpoint = format!("opc.tcp://127.0.0.1:{opc_port}");
    let (session, event_loop) = tokio::time::timeout(
        Duration::from_secs(15),
        client.connect_to_matching_endpoint(
            (
                endpoint.as_str(),
                SecurityPolicy::None.to_str(),
                MessageSecurityMode::None,
                UserTokenPolicy::anonymous(),
            ),
            IdentityToken::Anonymous,
        ),
    )
    .await
    .expect("endpoint connect timed out")
    .expect("endpoint connect failed");
    let loop_handle = event_loop.spawn();
    tokio::time::timeout(Duration::from_secs(10), session.wait_for_connection())
        .await
        .expect("session connect timed out");

    // ---- F3: connected-clients API reflects the live session ----
    assert_eq!(opcua.session_count(), 1, "one live session after connect");
    let auths = opcua.recent_authentications();
    assert!(
        auths
            .iter()
            .any(|a| a.user == "ANONYMOUS" && a.security_policy == "None"),
        "anonymous login recorded: {auths:?}"
    );

    let ns = 2u16; // first custom namespace: urn:opc-modbus-server:tags
    let read_ids = |names: &[&str]| -> Vec<ReadValueId> {
        names.iter().map(|n| NodeId::new(ns, *n).into()).collect()
    };

    // Poll the read until the poller has delivered real values.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (temp, speed) = loop {
        let results = session
            .read(
                &read_ids(&["plc1.temp", "plc1.speed"]),
                TimestampsToReturn::Source,
                0.0,
            )
            .await
            .expect("read service");
        let good = results
            .iter()
            .all(|dv| dv.status.unwrap_or(StatusCode::Bad).is_good());
        if good {
            break (results[0].clone(), results[1].clone());
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "values never became Good over OPC UA: {results:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // temp: i16 raw 237 scaled by 0.1 -> Double 23.7
    match temp.value {
        Some(Variant::Double(f)) => assert!((f - 23.7).abs() < 1e-9, "temp {f}"),
        other => panic!("temp: expected Double, got {other:?}"),
    }
    assert!(temp.source_timestamp.is_some(), "source timestamp present");
    // speed: u16 raw 25 through formula raw*60 -> Double 1500
    match speed.value {
        Some(Variant::Double(f)) => assert!((f - 1500.0).abs() < 1e-9, "speed {f}"),
        other => panic!("speed: expected Double, got {other:?}"),
    }

    // ---- live update: change the register, the OPC UA read must follow ----
    banks.lock().unwrap().get_mut(&1).unwrap().insert(1, 30); // 30 * 60 = 1800
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let dv = session
            .read(&read_ids(&["plc1.speed"]), TimestampsToReturn::Source, 0.0)
            .await
            .expect("read")
            .remove(0);
        if let Some(Variant::Double(f)) = dv.value {
            if (f - 1800.0).abs() < 1e-9 {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "updated value never arrived: {dv:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ---- phase 4: write from the OPC UA client back to the "device" ----
    use opcua::types::{AttributeId, DataValue, WriteValue};
    let write_value = |name: &str, v: Variant| WriteValue {
        node_id: NodeId::new(ns, name),
        attribute_id: AttributeId::Value as u32,
        index_range: Default::default(),
        value: DataValue::new_now(v),
    };

    // 45.3 degC-like setpoint: inverse of scale 0.1 -> raw 453 on the wire.
    let statuses = session
        .write(&[write_value("plc1.setpoint", Variant::Double(45.3))])
        .await
        .expect("write service");
    assert!(statuses[0].is_good(), "write must be device-acknowledged: {statuses:?}");
    assert_eq!(
        banks.lock().unwrap()[&1][&5],
        453,
        "inverse-transformed value reached the Modbus register"
    );

    // Read-back through the normal poll cycle confirms over OPC UA.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let dv = session
            .read(&read_ids(&["plc1.setpoint"]), TimestampsToReturn::Source, 0.0)
            .await
            .expect("read")
            .remove(0);
        if let Some(Variant::Double(f)) = dv.value {
            if (f - 45.3).abs() < 1e-6 {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "written setpoint never read back: {dv:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Non-writable tag must be rejected without touching the device.
    let before = banks.lock().unwrap()[&1][&0];
    let statuses = session
        .write(&[write_value("plc1.temp", Variant::Double(99.9))])
        .await
        .expect("write service call itself succeeds");
    assert!(!statuses[0].is_good(), "non-writable tag must reject: {statuses:?}");
    assert_eq!(banks.lock().unwrap()[&1][&0], before, "device register untouched");

    // ---- teardown ----
    let _ = session.disconnect().await;
    loop_handle.abort();

    // F3: after a clean disconnect (CloseSession) the live count drops.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while opcua.session_count() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "session count did not drop to 0 after disconnect"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    opcua.shutdown().await;
    engine_handle.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), poller.shutdown())
        .await
        .expect("poller shutdown");
}
