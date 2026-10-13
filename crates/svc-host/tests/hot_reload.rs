//! Local simulator acceptance: live session/subscription, reconfigured formulas,
//! writes and tag identities. No physical device is used.
use opcua::client::{ClientBuilder, DataChangeCallback, IdentityToken};
use opcua::crypto::SecurityPolicy;
use opcua::types::{
    AttributeId, DataValue, MessageSecurityMode, NodeId, ReadValueId, StatusCode,
    TimestampsToReturn, UserTokenPolicy, Variant, WriteValue,
};
use serde_json::{json, Value};
use std::future;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tags_core::{TypedReader, TypedValue};
use tokio::net::TcpListener;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::tcp::{accept_tcp_connection, Server},
    SlaveRequest,
};

#[derive(Clone)]
struct Slave(Arc<Mutex<Vec<u16>>>);
impl tokio_modbus::server::Service for Slave {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;
    fn call(&self, req: Self::Request) -> Self::Future {
        let mut bank = self.0.lock().unwrap();
        future::ready(match req.request {
            Request::ReadHoldingRegisters(addr, count) => bank
                .get(addr as usize..addr as usize + count as usize)
                .map(|v| Some(Response::ReadHoldingRegisters(v.to_vec())))
                .ok_or(ExceptionCode::IllegalDataAddress),
            Request::WriteSingleRegister(addr, value) => match bank.get_mut(addr as usize) {
                Some(v) => {
                    *v = value;
                    Ok(Some(Response::WriteSingleRegister(addr, value)))
                }
                None => Err(ExceptionCode::IllegalDataAddress),
            },
            _ => Err(ExceptionCode::IllegalFunction),
        })
    }
}

fn reg(name: &str, address: u16, formula: Option<&str>) -> Value {
    let mut reg = json!({"tag": name, "address": address, "poll_group":"fast", "function":"read_holding_registers", "data_type":"u16"});
    if let Some(f) = formula {
        reg["formula"] = json!(f);
    }
    reg
}
fn resolved(cfg: &Value) -> gateway_config::ResolvedConfig {
    gateway_config::load_str(&cfg.to_string()).unwrap()
}

async fn wait_typed(stack: &svc_host::RunningStack, name: &str, expected: f64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let store = stack.typed();
            if store
                .resolve(name)
                .and_then(|id| store.snapshot(id))
                .is_some_and(|s| {
                    s.quality == mb_poller::Quality::Good && s.value.as_f64() == Some(expected)
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("typed value must arrive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reload_keeps_session_subscription_and_updates_write_destination() {
    let dir = tempfile::tempdir().unwrap();
    let bank = Arc::new(Mutex::new(vec![10, 3, 0, 42, 77]));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mb_port = listener.local_addr().unwrap().port();
    let connections = Arc::new(AtomicUsize::new(0));
    let sim = {
        let service = Slave(bank.clone());
        let connections = connections.clone();
        tokio::spawn(async move {
            let on_connected = move |stream, addr| {
                connections.fetch_add(1, Ordering::SeqCst);
                let service = service.clone();
                async move { accept_tcp_connection(stream, addr, move |_| Ok(Some(service.clone()))) }
            };
            Server::new(listener)
                .serve(&on_connected, |_| {})
                .await
                .unwrap();
        })
    };
    let endpoint_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let opc_port = endpoint_listener.local_addr().unwrap().port();
    drop(endpoint_listener);
    let mut b = reg("b", 1, Some("raw + tag(\"a\")"));
    b["writable"] = json!(true);
    b["write_formula"] = json!("value - 10");
    let mut cfg = json!({
        "schema_version":"1", "gateway":{"data_dir":dir.path().join("data")},
        "opcua":{"host":"127.0.0.1","port":opc_port,"pki_dir":dir.path().join("pki"),"basic256sha256":false},
        "poll_groups":[{"id":"fast","period_ms":30}],
        "channels":[{"id":"tcp","transport":{"type":"tcp","host":"127.0.0.1","port":mb_port},"request_timeout_ms":100,
            "retry":{"max_retries":0,"base_backoff_ms":20,"max_backoff_ms":100},
            "devices":[{"id":"dev","unit_id":1,"registers":[reg("c",2,Some("tag(\"b\") * 2")),b,reg("a",0,None)]}]}]
    });
    let mut stack = svc_host::start_stack(resolved(&cfg)).await.unwrap();
    wait_typed(&stack, "c", 26.0).await;
    let mut client = ClientBuilder::new()
        .application_name("hot reload test")
        .application_uri("urn:hot-reload:test")
        .trust_server_certs(true)
        .create_sample_keypair(false)
        .pki_dir(dir.path().join("client").to_string_lossy().to_string())
        .session_retry_limit(0)
        .client()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let endpoint = format!("opc.tcp://127.0.0.1:{opc_port}");
    let (session, event_loop) = tokio::time::timeout(
        Duration::from_secs(10),
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
    .unwrap()
    .unwrap();
    let loop_handle = event_loop.spawn();
    tokio::time::timeout(Duration::from_secs(10), session.wait_for_connection())
        .await
        .unwrap();
    let (tx, mut notifications) = tokio::sync::mpsc::unbounded_channel();
    let sub = session
        .create_subscription(
            Duration::from_millis(50),
            100,
            10,
            0,
            0,
            true,
            DataChangeCallback::new(move |dv, _| {
                if let Some(Variant::UInt64(v)) = dv.value {
                    let _ = tx.send(v);
                }
            }),
        )
        .await
        .unwrap();
    session
        .create_monitored_items(
            sub,
            TimestampsToReturn::Both,
            vec![NodeId::new(2, "a").into()],
        )
        .await
        .unwrap();

    cfg["channels"][0]["devices"][0]["registers"][1]["formula"] = json!("raw + tag(\"a\") * 2");
    cfg["channels"][0]["devices"][0]["registers"][1]["write_formula"] = json!("value - 20");
    stack.reload(resolved(&cfg)).await.unwrap();
    wait_typed(&stack, "c", 46.0).await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "formula edit must keep Modbus connection"
    );
    assert_eq!(stack.opcua().unwrap().session_count(), 1);
    let read: Vec<ReadValueId> = vec![NodeId::new(2, "c").into()];
    let values = session
        .read(&read, TimestampsToReturn::Source, 0.0)
        .await
        .unwrap();
    assert_eq!(values[0].value, Some(Variant::Double(46.0)));
    let write = |name: &str, value: f64| WriteValue {
        node_id: NodeId::new(2, name),
        attribute_id: AttributeId::Value as u32,
        index_range: Default::default(),
        value: DataValue::new_now(Variant::Double(value)),
    };
    assert!(session.write(&[write("b", 28.0)]).await.unwrap()[0].is_good());
    assert_eq!(
        bank.lock().unwrap()[1],
        8,
        "new inverse formula must be active"
    );
    wait_typed(&stack, "c", 56.0).await;
    bank.lock().unwrap()[0] = 12;
    tokio::time::timeout(Duration::from_secs(5), async {
        while notifications.recv().await != Some(12) {}
    })
    .await
    .expect("original subscription must survive reload");

    let revision = stack.revision();
    let mut invalid = cfg.clone();
    invalid["channels"][0]["devices"][0]["registers"][1]["formula"] = json!("tag(\"missing\")");
    assert!(stack
        .reload(resolved(&invalid))
        .await
        .unwrap_err()
        .contains("missing"));
    assert_eq!(stack.revision(), revision);
    wait_typed(&stack, "c", 64.0).await;
    invalid = cfg.clone();
    invalid["channels"][0]["devices"][0]["registers"][2]["formula"] = json!("tag(\"b\")");
    assert!(stack
        .reload(resolved(&invalid))
        .await
        .unwrap_err()
        .contains("cycle"));

    // Remove c, insert x ahead of a and relocate writable b to address 4.
    // Dense IDs change; NodeIds and the client session must not.
    let mut b = cfg["channels"][0]["devices"][0]["registers"][1].clone();
    b["address"] = json!(4);
    cfg["channels"][0]["devices"][0]["registers"] =
        json!([reg("x", 3, None), reg("a", 0, None), b]);
    stack.reload(resolved(&cfg)).await.unwrap();
    wait_typed(&stack, "x", 42.0).await;
    wait_typed(&stack, "b", 101.0).await;
    let read: Vec<ReadValueId> = ["c", "x", "a"]
        .iter()
        .map(|n| NodeId::new(2, *n).into())
        .collect();
    let values = session
        .read(&read, TimestampsToReturn::Source, 0.0)
        .await
        .unwrap();
    assert_eq!(values[0].status, Some(StatusCode::BadNodeIdUnknown));
    assert_eq!(values[1].value, Some(Variant::UInt64(42)));
    assert_eq!(values[2].value, Some(Variant::UInt64(12)));
    assert!(session.write(&[write("b", 30.0)]).await.unwrap()[0].is_good());
    assert_eq!(bank.lock().unwrap()[4], 10, "write must use new address");
    assert_eq!(
        bank.lock().unwrap()[1],
        8,
        "retired address must not receive new writes"
    );
    assert_eq!(stack.opcua().unwrap().session_count(), 1);
    assert_eq!(
        stack.opcua().unwrap().recent_authentications().len(),
        1,
        "no reconnect/login during reload"
    );
    let _ = session.disconnect().await;
    loop_handle.abort();
    stack.shutdown().await;
    sim.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_watcher_rejects_invalid_then_applies_repaired_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let bank = Arc::new(Mutex::new(vec![7]));
    let service = Slave(bank);
    let sim = tokio::spawn(async move {
        let on_connected = move |stream, addr| {
            let service = service.clone();
            async move { accept_tcp_connection(stream, addr, move |_| Ok(Some(service.clone()))) }
        };
        Server::new(listener)
            .serve(&on_connected, |_| {})
            .await
            .unwrap();
    });
    let base = json!({"schema_version":"1","gateway":{"data_dir":"data"},"opcua":{"enabled":false},"poll_groups":[{"id":"fast","period_ms":40}],"channels":[{"id":"c","transport":{"type":"tcp","host":"127.0.0.1","port":port},"devices":[{"id":"d","unit_id":1,"registers":[reg("a",0,None)]}]}]});
    std::fs::write(&path, base.to_string()).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let cfg = gateway_config::load(&path).unwrap();
    let runner = tokio::spawn(svc_host::run_stack_watched(cfg, path.clone(), async {
        let _ = rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(650)).await;
    std::fs::write(&path, "{").unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        !runner.is_finished(),
        "invalid JSON must not terminate server"
    );
    let mut fixed = base.clone();
    fixed["channels"][0]["devices"][0]["registers"][0]["retentive"] = json!(true);
    fixed["channels"][0]["devices"][0]["registers"][0]["formula"] = json!("raw * 3");
    std::fs::write(&path, fixed.to_string()).unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    tx.send(()).unwrap();
    assert_eq!(runner.await.unwrap(), svc_host::StackExit::Clean);
    let persist = tags_core::Persist::open(dir.path().join("data/tags.sled")).unwrap();
    let (value, _) = persist
        .load_retentive("a")
        .unwrap()
        .expect("repaired config must have been applied")
        .to_typed();
    assert_eq!(value, TypedValue::Float(21.0));
    sim.abort();
}
