//! Real MCP HTTP framing and real Modbus TCP exchanges on local simulators.
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use svc_host::mcp::Controller;
use tokio::net::TcpListener;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::tcp::{accept_tcp_connection, Server},
    SlaveRequest,
};

#[derive(Clone)]
struct Slave {
    banks: Arc<Mutex<HashMap<u8, Vec<u16>>>>,
    polls: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
}
impl tokio_modbus::server::Service for Slave {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;
    fn call(&self, req: Self::Request) -> Self::Future {
        let mut banks = self.banks.lock().unwrap();
        let Some(bank) = banks.get_mut(&req.slave) else {
            return future::ready(Err(ExceptionCode::GatewayTargetDevice));
        };
        future::ready(match req.request {
            Request::ReadHoldingRegisters(addr, count) => {
                if req.slave == 1 && count > 1 {
                    self.polls.fetch_add(1, Ordering::SeqCst);
                }
                bank.get(addr as usize..addr as usize + count as usize)
                    .map(|v| Some(Response::ReadHoldingRegisters(v.to_vec())))
                    .ok_or(ExceptionCode::IllegalDataAddress)
            }
            Request::WriteSingleRegister(addr, value) => match bank.get_mut(addr as usize) {
                Some(v) => {
                    *v = value;
                    self.writes.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(Response::WriteSingleRegister(addr, value)))
                }
                None => Err(ExceptionCode::IllegalDataAddress),
            },
            Request::WriteMultipleRegisters(addr, values) => {
                if let Some(target) = bank.get_mut(addr as usize..addr as usize + values.len()) {
                    target.copy_from_slice(&values);
                    self.writes.fetch_add(1, Ordering::SeqCst);
                    Ok(Some(Response::WriteMultipleRegisters(
                        addr,
                        values.len() as u16,
                    )))
                } else {
                    Err(ExceptionCode::IllegalDataAddress)
                }
            }
            _ => Err(ExceptionCode::IllegalFunction),
        })
    }
}
fn config(dir: &std::path::Path, port: u16) -> Value {
    json!({
        "schema_version":"1","gateway":{"data_dir":dir.join("data")},"mcp":{"port":0},
        "opcua":{"enabled":false,"users":[{"username":"test-only","password":"private-test-password"}]},
        "poll_groups":[{"id":"fast","period_ms":20}],
        "channels":[{"id":"tcp","transport":{"type":"tcp","host":"127.0.0.1","port":port},"request_timeout_ms":100,
            "retry":{"max_retries":0,"base_backoff_ms":20,"max_backoff_ms":100},
            "devices":[{"id":"d","unit_id":1,"registers":[
                {"tag":"a","poll_group":"fast","address":0,"function":"read_holding_registers","data_type":"u16","writable":true,"deadband":10.0},
                {"tag":"b","poll_group":"fast","address":1,"function":"read_holding_registers","data_type":"u16","formula":"raw + tag(\"a\")"}
            ]}]}]
    })
}
async fn rpc(owner: &Controller, request: Value, auth: bool, origin: Option<&str>) -> (u16, Value) {
    let mut sidecar = owner.path.as_os_str().to_os_string();
    sidecar.push(".mcp-endpoint.json");
    let info: Value =
        serde_json::from_slice(&std::fs::read(std::path::PathBuf::from(sidecar)).unwrap()).unwrap();
    let address = info["url"]
        .as_str()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap();
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut req = hyper::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", address)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream");
    if auth {
        req = req.header(
            "Authorization",
            format!("Bearer {}", info["token"].as_str().unwrap()),
        );
    }
    if let Some(origin) = origin {
        req = req.header("Origin", origin);
    }
    let reply = client
        .send_request(
            req.body(Full::new(Bytes::from(request.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = reply.status().as_u16();
    let bytes = reply.into_body().collect().await.unwrap().to_bytes();
    task.abort();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn tool(owner: &Controller, name: &str, args: Value) -> Value {
    let(status,reply)=rpc(owner,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}),true,None).await;
    assert_eq!(status, 200);
    assert_eq!(reply["result"]["isError"], false, "{name}: {reply}");
    reply["result"]["structuredContent"].clone()
}
async fn wait_value(owner: &Arc<Controller>, name: &str, expected: f64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = owner
                .call("read_tags", json!({"filter":name}))
                .await
                .unwrap();
            if result["tags"].as_array().unwrap().iter().any(|v| {
                v["calculated"]["tag"] == name
                    && v["calculated"]["value"].as_f64() == Some(expected)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("calculated value must arrive");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authenticated_http_configuration_discovery_and_writes_share_one_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let banks = Arc::new(Mutex::new(HashMap::from([
        (1, vec![5, 3, 11, 12]),
        (7, vec![77]),
        (8, vec![]),
    ])));
    let polls = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let connections = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sim = {
        let service = Slave {
            banks: banks.clone(),
            polls: polls.clone(),
            writes: writes.clone(),
        };
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
    std::fs::write(&path, config(dir.path(), port).to_string()).unwrap();
    let owner = Controller::new(&path).await.unwrap();
    assert_eq!(
        rpc(
            &owner,
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            false,
            None
        )
        .await
        .0,
        403
    );
    assert_eq!(
        rpc(
            &owner,
            json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
            true,
            Some("https://untrusted.example")
        )
        .await
        .0,
        403
    );
    let(_,init)=rpc(&owner,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"wire-test","version":"1"}}}),true,None).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    let (_, list) = rpc(
        &owner,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        true,
        None,
    )
    .await;
    assert!(list["result"]["tools"].as_array().unwrap().len() >= 30);
    let read = tool(&owner, "get_config", json!({})).await;
    let revision = read["revision"].as_str().unwrap();
    assert_eq!(
        read["config"]["opcua"]["users"][0]["password"],
        "<redacted>"
    );
    assert!(!read.to_string().contains("private-test-password"));
    let before = std::fs::read(&path).unwrap();
    assert!(owner
        .call(
            "set_formula",
            json!({"tag":"b","formula":"raw * 2","expected_revision":"stale"})
        )
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let preview=tool(&owner,"set_formula",json!({"tag":"b","formula":"raw + tag(\"a\") * 2","expected_revision":revision,"dry_run":true})).await;
    assert_eq!(preview["saved"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let update = tool(
        &owner,
        "set_formula",
        json!({"tag":"b","formula":"raw + tag(\"a\") * 2","expected_revision":revision}),
    )
    .await;
    assert_eq!(update["saved"], true);
    assert_eq!(update["applied"], false);
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    assert_eq!(
        std::fs::read(std::path::PathBuf::from(backup)).unwrap(),
        before
    );
    tool(&owner, "server_start", json!({})).await;
    wait_value(&owner, "b", 13.0).await;
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    banks.lock().unwrap().get_mut(&1).unwrap()[0] = 6;
    wait_value(&owner, "b", 15.0).await;
    let formula = tool(
        &owner,
        "test_formula",
        json!({"formula":"tag(\"a\") * 2","input":0}),
    )
    .await;
    assert_eq!(formula["result"], 12.0);
    let status = tool(&owner, "server_status", json!({})).await;
    let active = status["applied_revision"].as_str().unwrap();
    assert!(owner
        .call(
            "write_tag",
            json!({"tag":"a","value":8,"expected_revision":active})
        )
        .await
        .is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    tool(
        &owner,
        "write_tag",
        json!({"tag":"a","value":8,"expected_revision":active,"confirm":true}),
    )
    .await;
    assert_eq!(banks.lock().unwrap()[&1][0], 8);
    tool(&owner,"write_modbus",json!({"channel_id":"tcp","device_id":"d","function":16,"address":2,"values":[21,22],"expected_revision":active,"confirm":true})).await;
    assert_eq!(&banks.lock().unwrap()[&1][2..], &[21, 22]);
    let raw = tool(
        &owner,
        "read_modbus",
        json!({"channel_id":"tcp","unit_id":7,"function":3,"address":0,"quantity":1}),
    )
    .await;
    assert_eq!(raw["response"]["registers"], json!([77]));
    let polls_before = polls.load(Ordering::SeqCst);
    let scan = tool(
        &owner,
        "start_scan",
        json!({"channel_id":"tcp","unit_start":1,"unit_end":8,"timeout_ms":100,"delay_ms":20}),
    )
    .await;
    let id = scan["job_id"].as_str().unwrap();
    let done = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = owner
                .call("scan_status", json!({"job_id":id}))
                .await
                .unwrap();
            if value["status"] != "running" {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(done["status"], "completed");
    let mut found: Vec<_> = done["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["unit_id"].as_u64().unwrap())
        .collect();
    found.sort();
    assert_eq!(found, vec![1, 7, 8]);
    assert!(
        polls.load(Ordering::SeqCst) > polls_before,
        "discovery must not starve scheduled reads"
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "MCP must share the live connection, not open a second socket"
    );
    let current = tool(&owner, "get_config", json!({})).await;
    let rev = current["revision"].as_str().unwrap();
    assert!(owner
        .call(
            "set_formula",
            json!({"tag":"a","formula":"tag(\"b\")","expected_revision":rev})
        )
        .await
        .is_err());
    let result = tool(
        &owner,
        "set_formula",
        json!({"tag":"b","formula":"raw + tag(\"a\") * 3","expected_revision":rev}),
    )
    .await;
    assert_eq!(result["applied"], true);
    wait_value(&owner, "b", 27.0).await;
    let observer = svc_host::mcp::Client::connect(&path).await.unwrap();
    let view = observer.view().await.unwrap();
    assert!(view.running);
    let store = view.typed.unwrap();
    use tags_core::TypedReader;
    let value = store.snapshot(store.resolve("b").unwrap()).unwrap();
    assert_eq!(value.value.as_f64(), Some(27.0));
    assert_eq!(value.quality, mb_poller::Quality::Good);
    assert_eq!(view.saved_revision, view.applied_revision);
    assert_eq!(connections.load(Ordering::SeqCst), 1);
    assert!(owner.call("server_stop", json!({})).await.is_err());
    tool(&owner, "server_stop", json!({"confirm":true})).await;
    assert_eq!(
        tool(&owner, "server_status", json!({})).await["running"],
        false
    );
    assert!(owner.url().is_some());
    let audit = tool(&owner, "get_audit", json!({})).await;
    assert!(!audit.to_string().contains("private-test-password"));
    assert!(audit["operations"].as_array().unwrap().len() > 10);
    let address = owner
        .url()
        .unwrap()
        .strip_prefix("http://")
        .unwrap()
        .strip_suffix("/mcp")
        .unwrap()
        .to_owned();
    owner.shutdown().await;
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    sim.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_scan_cancellation_releases_lease_before_starting_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accept TCP but never answer Modbus: cancellation has to finish the
    // bounded transaction rather than releasing a live half-written request.
    let sim = tokio::spawn(async move {
        let mut sockets = vec![];
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            sockets.push(stream);
        }
    });
    std::fs::write(&path, config(dir.path(), port).to_string()).unwrap();
    let owner = Controller::new(&path).await.unwrap();
    let scan = owner
        .call(
            "start_scan",
            json!({"channel_id":"tcp","unit_start":1,"unit_end":247,"timeout_ms":100}),
        )
        .await
        .unwrap();
    let id = scan["job_id"].as_str().unwrap();
    assert!(owner.start_server().await.unwrap_err().contains("scan"));
    owner
        .call("cancel_scan", json!({"job_id":id}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner
            .call("scan_status", json!({"job_id":id}))
            .await
            .unwrap()["status"]
            == "running"
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        owner
            .call("scan_status", json!({"job_id":id}))
            .await
            .unwrap()["status"],
        "cancelled"
    );
    owner.start_server().await.unwrap();
    owner.shutdown().await;
    sim.abort();
}
