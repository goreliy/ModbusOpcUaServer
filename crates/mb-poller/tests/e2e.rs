//! Stage-9 end-to-end proof (design §10 item 9), against the shared
//! in-process multi-slave simulator:
//!
//! (a) coalescing actually reduces the request count (reqs_ok grows far
//!     slower than tags-read-per-tick would imply),
//! (b) fast-group tags update more often than slow-group tags (seq deltas),
//! (c) the simulator dying mid-run flips tags Bad, a restart recovers Good,
//! (d) shutdown joins all channel tasks within a timeout.

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{Poller, Quality};
use mb_types::ChannelId;
use tokio::net::TcpListener;

use sim::{bind, snap, spawn_server, wait_for, Banks};

/// 4 contiguous fast tags (one coalesced FC03 x4) + 1 slow tag.
const FAST_TAGS: [&str; 4] = ["f.0", "f.1", "f.2", "f.3"];
const SLOW_TAG: &str = "s.0";
const ALL_TAGS: [&str; 5] = ["f.0", "f.1", "f.2", "f.3", "s.0"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coalescing_rates_outage_recovery_shutdown() {
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0, 10), (1, 20), (2, 30), (3, 40), (20, 555)]),
    )])));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut server = spawn_server(listener, Arc::clone(&banks));

    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [
            {{ "id": "fast", "period_ms": 100 }},
            {{ "id": "slow", "period_ms": 500 }}
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
                    {{ "tag": "f.0", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }},
                    {{ "tag": "f.1", "poll_group": "fast", "function": "read_holding_registers", "address": 1, "data_type": "u16" }},
                    {{ "tag": "f.2", "poll_group": "fast", "function": "read_holding_registers", "address": 2, "data_type": "u16" }},
                    {{ "tag": "f.3", "poll_group": "fast", "function": "read_holding_registers", "address": 3, "data_type": "u16" }},
                    {{ "tag": "s.0", "poll_group": "slow", "function": "read_holding_registers", "address": 20, "data_type": "u16" }}
                ] }}
            ]
        }} ]
    }}"#,
        port = addr.port()
    ))
    .expect("config loads");

    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    let ch = ChannelId(0);
    let metrics = handle.metrics(ch).expect("channel metrics");

    // ---- warm-up: everything Good at least once. ----
    wait_for("all tags initially Good", Duration::from_secs(5), || {
        ALL_TAGS.iter().all(|t| snap(&cache, t).quality == Quality::Good)
    })
    .await;

    // ---- steady-state measurement window (~1.5 s of real polling). ----
    let m0 = metrics.snapshot();
    let fast0 = snap(&cache, FAST_TAGS[0]).seq;
    let slow0 = snap(&cache, SLOW_TAG).seq;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let m1 = metrics.snapshot();
    let fast_delta = snap(&cache, FAST_TAGS[0]).seq - fast0;
    let slow_delta = snap(&cache, SLOW_TAG).seq - slow0;
    let reqs_delta = m1.reqs_ok - m0.reqs_ok;

    // Sanity: the window saw real polling on both groups.
    assert!(fast_delta >= 5, "fast group barely polled: {fast_delta}");
    assert!(slow_delta >= 1, "slow group never polled: {slow_delta}");

    // (b) fast tags update strictly more often than slow tags (5x period).
    assert!(
        fast_delta >= 2 * slow_delta,
        "fast group must outpace slow group: fast={fast_delta} slow={slow_delta}"
    );

    // (a) coalescing: 4 fast tags rode ONE request per tick. Uncoalesced, the
    // window would have cost >= 4*fast + slow requests; require at most half.
    let uncoalesced = 4 * fast_delta + slow_delta;
    assert!(
        2 * reqs_delta <= uncoalesced,
        "coalescing did not reduce request count: reqs={reqs_delta} vs uncoalesced estimate {uncoalesced}"
    );
    // ...but every fast tick still costs at least one wire request (+1 slack
    // for metric/snapshot read skew at the window edges).
    assert!(
        reqs_delta + 1 >= fast_delta,
        "request counter implausibly low: reqs={reqs_delta} fast={fast_delta}"
    );

    // All four fast tags advance in lockstep (published from one response).
    let seqs: Vec<u64> = FAST_TAGS.iter().map(|t| snap(&cache, t).seq).collect();
    let spread = seqs.iter().max().unwrap() - seqs.iter().min().unwrap();
    assert!(spread <= 1, "coalesced tags must advance together: {seqs:?}");

    // ---- (c) simulator dies mid-run: Bad sweep, last values kept... ----
    server.abort();
    let _ = (&mut server).await;
    wait_for("all tags Bad after server death", Duration::from_secs(10), || {
        ALL_TAGS.iter().all(|t| snap(&cache, t).quality == Quality::Bad)
    })
    .await;

    // ...then a restart on the same port recovers everything to Good.
    let listener = bind(addr).await;
    let server = spawn_server(listener, Arc::clone(&banks));
    wait_for("recovery to Good after restart", Duration::from_secs(10), || {
        ALL_TAGS.iter().all(|t| snap(&cache, t).quality == Quality::Good)
    })
    .await;
    assert!(
        metrics.snapshot().reconnects >= 1,
        "outage must be visible in reconnects"
    );

    // ---- (d) shutdown joins every channel task within the timeout. ----
    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown must join all channel tasks within 5s");
    server.abort();
}
