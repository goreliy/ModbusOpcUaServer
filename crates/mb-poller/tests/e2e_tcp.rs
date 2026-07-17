//! End-to-end: `Poller::spawn_with_cache` against an in-process tokio-modbus
//! TCP server that dispatches on **unit id** (`SlaveRequest` service), one
//! channel, two devices, fast/slow poll groups.
//!
//! Covers: coalesced values landing Good in the cache; a `WriteCommand`
//! round-trip; server death -> whole-channel Bad sweep with last values
//! retained; reconnect -> recovery back to Good; metrics sanity; shutdown.

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{Poller, Quality, RawValue, WriteCommand};
use mb_proto::{ModbusRequest, ModbusResponse};
use mb_types::{ChannelId, DeviceId};
use tokio::net::TcpListener;

use sim::{bind, good_u16, snap, spawn_server, wait_for, Banks};

const TAGS: [&str; 3] = ["d1.a", "d1.b", "d2.a"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn poll_write_die_reconnect_recover() {
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([
        (1, HashMap::from([(0, 111), (1, 222)])),
        (2, HashMap::from([(10, 333)])),
    ])));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut server = spawn_server(listener, Arc::clone(&banks));

    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [
            {{ "id": "fast", "period_ms": 50 }},
            {{ "id": "slow", "period_ms": 150 }}
        ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 250,
            "offline_after_failures": 2,
            "retry": {{ "max_retries": 0, "base_backoff_ms": 50, "max_backoff_ms": 200 }},
            "max_gap": 1,
            "devices": [
                {{ "id": "dev1", "unit_id": 1, "registers": [
                    {{ "tag": "d1.a", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }},
                    {{ "tag": "d1.b", "poll_group": "fast", "function": "read_holding_registers", "address": 1, "data_type": "u16" }}
                ] }},
                {{ "id": "dev2", "unit_id": 2, "registers": [
                    {{ "tag": "d2.a", "poll_group": "slow", "function": "read_holding_registers", "address": 10, "data_type": "u16" }}
                ] }}
            ]
        }} ]
    }}"#,
        port = addr.port()
    ))
    .expect("config loads");

    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    let ch = ChannelId(0);

    // ---- 1. Values land Good (d1.a/d1.b arrive via ONE coalesced read). ----
    wait_for("initial Good values", Duration::from_secs(5), || {
        good_u16(&cache, "d1.a", 111) && good_u16(&cache, "d1.b", 222) && good_u16(&cache, "d2.a", 333)
    })
    .await;

    // ---- 2. WriteCommand round-trip. ----
    let writer = handle.writer(ch).expect("channel writer");
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    writer
        .send(WriteCommand {
            device: DeviceId(0),
            req: ModbusRequest::WriteSingleRegister { addr: 0, value: 999 },
            reply: reply_tx,
            deadline: std::time::Instant::now() + Duration::from_secs(60),
        })
        .await
        .expect("write queued");
    let resp = reply_rx.await.expect("reply").expect("write ok");
    assert!(matches!(resp, ModbusResponse::WriteAck));
    assert_eq!(banks.lock().unwrap()[&1][&0], 999, "write reached the slave");
    wait_for("written value polled back", Duration::from_secs(5), || {
        good_u16(&cache, "d1.a", 999)
    })
    .await;

    // ---- 3. Kill the server: everything flips Bad, last values retained. ----
    server.abort();
    let _ = (&mut server).await;
    wait_for("all tags Bad after server death", Duration::from_secs(10), || {
        TAGS.iter().all(|t| snap(&cache, t).quality == Quality::Bad)
    })
    .await;
    // SCADA convention: show last value + Bad quality, never a fake 0.
    assert_eq!(snap(&cache, "d1.a").value, RawValue::Registers(vec![999].into()));
    assert_eq!(snap(&cache, "d2.a").value, RawValue::Registers(vec![333].into()));

    // ---- 4. Restart on the same port: reconnect + recovery -> Good again. ----
    let listener = bind(addr).await;
    let server = spawn_server(listener, Arc::clone(&banks));
    wait_for("recovery to Good after restart", Duration::from_secs(10), || {
        good_u16(&cache, "d1.a", 999) && good_u16(&cache, "d1.b", 222) && good_u16(&cache, "d2.a", 333)
    })
    .await;

    // ---- 5. Metrics sanity. ----
    let m = handle.metrics(ch).expect("channel metrics").snapshot();
    assert!(m.reqs_ok > 0, "successful polls counted: {m:?}");
    assert!(m.reconnects >= 1, "the death/restart cycle counted: {m:?}");
    assert_eq!(m.writes_ok, 1, "{m:?}");

    // ---- 6. Graceful shutdown joins the channel task. ----
    handle.shutdown().await;
    server.abort();
}
