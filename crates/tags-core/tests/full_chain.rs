//! Full-chain integration: Modbus TCP simulator -> mb-poller -> TagCache ->
//! TagEngine -> TypedStore. The complete phase-1+2 data path in one test.

use std::collections::HashMap;
use std::future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{CacheReader, Poller, Quality};
use mb_types::TagId;
use tags_core::{Persist, TagEngine, TypedReader, TypedValue};
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

async fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simulator_to_typed_store_end_to_end() {
    // Slave 1: temp raw 237 (i16, scale 0.1 -> 23.7), speed raw 25 with a
    // formula (*60 -> 1500), and a coalesced neighbour used by a cross-tag
    // formula on another register.
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0u16, 237u16), (1, 25), (2, 10), (3, 5)]),
    )])));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = {
        let banks = Arc::clone(&banks);
        tokio::spawn(async move {
            let server = Server::new(listener);
            let service = MultiSlave { banks };
            let on_connected = move |stream, socket_addr| {
                let service = service.clone();
                async move {
                    accept_tcp_connection(stream, socket_addr, move |_| Ok(Some(service.clone())))
                }
            };
            let _ = server.serve(&on_connected, |err| eprintln!("server error: {err}")).await;
        })
    };

    let dir = tempfile::tempdir().unwrap();
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [ {{ "id": "fast", "period_ms": 50 }} ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 250,
            "retry": {{ "max_retries": 0, "base_backoff_ms": 50, "max_backoff_ms": 200 }},
            "devices": [ {{ "id": "dev1", "unit_id": 1, "registers": [
                {{ "tag": "temp",  "poll_group": "fast", "function": "read_holding_registers",
                   "address": 0, "data_type": "i16", "scale": 0.1, "retentive": true }},
                {{ "tag": "speed", "poll_group": "fast", "function": "read_holding_registers",
                   "address": 1, "data_type": "u16", "formula": "raw * 60" }},
                {{ "tag": "a",     "poll_group": "fast", "function": "read_holding_registers",
                   "address": 2, "data_type": "u16" }},
                {{ "tag": "total", "poll_group": "fast", "function": "read_holding_registers",
                   "address": 3, "data_type": "u16", "formula": "raw + tag(\"a\")" }}
            ] }} ]
        }} ]
    }}"#,
        port = addr.port()
    ))
    .expect("config loads");

    // Phase-1 stack.
    let (poller, cache) = Poller::spawn_with_cache(&cfg);
    // Phase-2 stack.
    let engine = TagEngine::new(
        &cfg,
        Arc::clone(&cache) as Arc<dyn CacheReader>,
        Some(Persist::open(dir.path()).unwrap()),
    )
    .unwrap();
    let (engine_handle, typed) = engine.spawn();

    // Engineering values appear, correctly decoded and transformed.
    wait_for("typed values Good", Duration::from_secs(5), || {
        let t = typed.snapshot(TagId(0));
        let s = typed.snapshot(TagId(1));
        matches!(&t, Some(s) if s.quality == Quality::Good)
            && matches!(&s, Some(s) if s.quality == Quality::Good)
    })
    .await;

    match typed.snapshot(typed.resolve("temp").unwrap()).unwrap().value {
        TypedValue::Float(f) => assert!((f - 23.7).abs() < 1e-9, "temp = {f}"),
        other => panic!("temp: expected Float, got {other:?}"),
    }
    assert_eq!(
        typed.snapshot(typed.resolve("speed").unwrap()).unwrap().value,
        TypedValue::Float(1500.0)
    );
    // Cross-tag: total = raw(5) + a(10).
    wait_for("cross-tag formula", Duration::from_secs(5), || {
        typed.snapshot(typed.resolve("total").unwrap()).unwrap().value == TypedValue::Float(15.0)
    })
    .await;

    // Kill the simulator: Bad propagates through to the typed layer,
    // last engineering values retained.
    server.abort();
    wait_for("typed Bad after server death", Duration::from_secs(10), || {
        typed.snapshot(TagId(0)).unwrap().quality == Quality::Bad
    })
    .await;
    match typed.snapshot(TagId(0)).unwrap().value {
        TypedValue::Float(f) => assert!((f - 23.7).abs() < 1e-9, "last value kept"),
        other => panic!("expected retained Float, got {other:?}"),
    }

    // Clean shutdown, then a NEW engine instance restores temp from sled.
    engine_handle.shutdown().await;
    tokio::time::timeout(Duration::from_secs(5), poller.shutdown())
        .await
        .expect("poller shutdown");

    let cfg2 = cfg; // same resolved config
    let cache2 = Arc::new(mb_poller::TagCache::new(&cfg2));
    let engine2 = TagEngine::new(
        &cfg2,
        cache2 as Arc<dyn CacheReader>,
        Some(Persist::open(dir.path()).unwrap()),
    )
    .unwrap();
    let typed2 = engine2.store();
    let restored = typed2.snapshot(typed2.resolve("temp").unwrap()).unwrap();
    assert_eq!(restored.quality, Quality::Uncertain, "restored, not fresh");
    match restored.value {
        TypedValue::Float(f) => assert!((f - 23.7).abs() < 1e-9, "restored {f}"),
        other => panic!("expected restored Float, got {other:?}"),
    }
}
