//! End-to-end adaptive de-coalescing (finding #16b): a coalesced read that
//! spans one UNMAPPED register gets `IllegalDataAddress` from the slave; the
//! poller must split it into per-field singletons (once), after which the
//! mapped tags recover to Good and keep updating, while the unmapped tag
//! stays Bad — and the device never counts as offline (it answered).

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{Poller, Quality};
use mb_types::ChannelId;
use tokio::net::TcpListener;

use sim::{good_u16, snap, spawn_server, wait_for, Banks};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalesced_read_with_hole_splits_and_recovers() {
    // Unit 1 maps addr 0 and 2; addr 1 is a hole -> IllegalDataAddress for
    // the coalesced qty=3 read, then per-field singletons succeed for 0 and 2.
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0u16, 11u16), (2, 33)]),
    )])));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = spawn_server(listener, Arc::clone(&banks));

    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [ {{ "id": "fast", "period_ms": 50 }} ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 250,
            "offline_after_failures": 2,
            "retry": {{ "max_retries": 0, "base_backoff_ms": 50, "max_backoff_ms": 200 }},
            "devices": [
                {{ "id": "dev1", "unit_id": 1, "registers": [
                    {{ "tag": "ok_a",    "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }},
                    {{ "tag": "missing", "poll_group": "fast", "function": "read_holding_registers", "address": 1, "data_type": "u16" }},
                    {{ "tag": "ok_b",    "poll_group": "fast", "function": "read_holding_registers", "address": 2, "data_type": "u16" }}
                ] }}
            ]
        }} ]
    }}"#,
        port = addr.port()
    ))
    .expect("config loads");

    let (handle, cache) = Poller::spawn_with_cache(&cfg);

    // The split must happen and the mapped tags recover Good.
    wait_for("mapped tags Good after de-coalesce", Duration::from_secs(5), || {
        good_u16(&cache, "ok_a", 11) && good_u16(&cache, "ok_b", 33)
    })
    .await;

    // The unmapped tag stays Bad (its singleton keeps answering
    // IllegalDataAddress); the device is NOT offline — it answers.
    assert_eq!(snap(&cache, "missing").quality, Quality::Bad);

    // The split is remembered: polling continues via the singletons, so the
    // mapped tags' seq keeps growing tick after tick.
    let seq_a0 = snap(&cache, "ok_a").seq;
    wait_for("ok_a keeps updating via the split", Duration::from_secs(5), || {
        snap(&cache, "ok_a").seq >= seq_a0 + 3
    })
    .await;
    assert_eq!(snap(&cache, "missing").quality, Quality::Bad, "hole stays Bad");

    // Metrics tell the same story: exceptions seen, but no reconnect churn
    // (an exception reply is proof of life, not a comm loss).
    let m = handle.metrics(ChannelId(0)).expect("metrics").snapshot();
    assert!(m.exceptions >= 1, "the coalesced read must have hit the hole");
    assert_eq!(m.reconnects, 0, "no reconnects: the device answered throughout");

    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown joins");
    server.abort();
}
