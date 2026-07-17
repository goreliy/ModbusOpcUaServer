//! End-to-end demo (design §10 item 9): an in-process multi-slave Modbus TCP
//! simulator (two unit ids behind one listener, register banks drifting in
//! the background), a config with a fast (200 ms) and a slow (2 s) poll
//! group, ~5 s of live polling with a compact tag table printed twice a
//! second from the `TagCache`, one FC06 write mid-run, final channel metrics,
//! and a clean `PollerHandle::shutdown`.
//!
//! Run with:
//!
//! ```text
//! cargo run -p mb-poller --example e2e_demo
//! ```

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{CacheReader, Poller, Quality, RawValue, TagCache, WriteCommand};
use mb_proto::ModbusRequest;
use mb_types::{ChannelId, DeviceId};
use tokio::net::TcpListener;
use tokio::sync::broadcast::error::RecvError;

fn quality_str(q: Quality) -> &'static str {
    match q {
        Quality::Good => "Good",
        Quality::Uncertain => "Uncertain",
        Quality::Bad => "Bad",
    }
}

fn value_str(v: &RawValue) -> String {
    match v {
        RawValue::Registers(r) => r
            .iter()
            .map(|w| w.to_string())
            .collect::<Vec<_>>()
            .join(","),
        RawValue::Bits(b) => b.iter().map(|x| if *x { '1' } else { '0' }).collect(),
        RawValue::Raw(bytes) => {
            format!("0x{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
        }
        RawValue::Absent => "-".into(),
    }
}

fn print_table(tag_names: &[String], cache: &TagCache) {
    println!(
        "{:<18} {:>8} {:>10} {:>8} {:>5}",
        "tag", "value", "quality", "age_ms", "seq"
    );
    for name in tag_names {
        let s = cache
            .resolve(name)
            .and_then(|id| cache.snapshot(id))
            .expect("tag exists in cache");
        println!(
            "{:<18} {:>8} {:>10} {:>8} {:>5}",
            name,
            value_str(&s.value),
            quality_str(s.quality),
            s.mono.elapsed().as_millis(),
            s.seq
        );
    }
}

#[tokio::main]
async fn main() {
    // --- simulator: two unit ids behind one TCP listener. ---
    let banks: sim::Banks = Arc::new(Mutex::new(HashMap::from([
        (1, HashMap::from([(0, 100), (1, 0), (2, 0)])),
        (2, HashMap::from([(10, 5000), (11, 0)])),
    ])));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let server = sim::spawn_server(listener, Arc::clone(&banks));

    // Background "process": registers drift so the live table moves.
    // (Register 0 of unit 1 is left alone — the mid-run write targets it.)
    let mutator = {
        let banks = Arc::clone(&banks);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(300));
            loop {
                tick.tick().await;
                let mut b = banks.lock().unwrap();
                if let Some(d1) = b.get_mut(&1) {
                    *d1.entry(1).or_default() += 1;
                    *d1.entry(2).or_default() += 3;
                }
                if let Some(d2) = b.get_mut(&2) {
                    *d2.entry(10).or_default() += 10;
                    *d2.entry(11).or_default() += 1;
                }
            }
        })
    };

    // --- config: fast group 200 ms, slow group 2 s, two devices. ---
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [
            {{ "id": "fast", "period_ms": 200 }},
            {{ "id": "slow", "period_ms": 2000 }}
        ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 500,
            "offline_after_failures": 2,
            "retry": {{ "max_retries": 1, "base_backoff_ms": 100, "max_backoff_ms": 1000 }},
            "max_gap": 1,
            "devices": [
                {{ "id": "plc1", "unit_id": 1, "registers": [
                    {{ "tag": "plc1.setpoint",    "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }},
                    {{ "tag": "plc1.temperature", "poll_group": "fast", "function": "read_holding_registers", "address": 1, "data_type": "u16" }},
                    {{ "tag": "plc1.flow",        "poll_group": "fast", "function": "read_holding_registers", "address": 2, "data_type": "u16" }}
                ] }},
                {{ "id": "meter", "unit_id": 2, "registers": [
                    {{ "tag": "meter.energy",  "poll_group": "slow", "function": "read_holding_registers", "address": 10, "data_type": "u16" }},
                    {{ "tag": "meter.counter", "poll_group": "slow", "function": "read_holding_registers", "address": 11, "data_type": "u16" }}
                ] }}
            ]
        }} ]
    }}"#,
        port = addr.port()
    ))
    .expect("demo config must load");

    let tag_names = cfg.tag_names.clone();
    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    let ch = ChannelId(0);

    println!("e2e_demo: simulator on {addr}; fast=200ms (plc1.* coalesced x3), slow=2000ms (meter.*); running ~5s\n");

    // Live change feed: one ChangeBatch per transaction, lossy on lag.
    let mut rx = cache.subscribe();
    let mut batches: u64 = 0;
    let start = tokio::time::Instant::now();
    let mut wrote = false;
    let mut round = 0u32;

    while start.elapsed() < Duration::from_secs(5) {
        // Drain change notifications for one 500 ms window...
        let window_end = tokio::time::Instant::now() + Duration::from_millis(500);
        loop {
            match tokio::time::timeout_at(window_end, rx.recv()).await {
                Ok(Ok(_batch)) => batches += 1,
                Ok(Err(RecvError::Lagged(missed))) => batches += missed,
                Ok(Err(RecvError::Closed)) => break,
                Err(_window_over) => break,
            }
        }
        // ...then print the live table.
        round += 1;
        println!(
            "--- t={:>4}ms  change-batches so far: {batches} ---",
            start.elapsed().as_millis()
        );
        print_table(&tag_names, &cache);
        println!();

        // Halfway through: exercise the write path (FC06 to unit 1, addr 0).
        if !wrote && round >= 5 {
            wrote = true;
            let writer = handle.writer(ch).expect("channel writer");
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            writer
                .send(WriteCommand {
                    device: DeviceId(0),
                    req: ModbusRequest::WriteSingleRegister { addr: 0, value: 4242 },
                    reply: reply_tx,
                    deadline: std::time::Instant::now() + Duration::from_secs(60),
                })
                .await
                .expect("write queued");
            match reply_rx.await {
                Ok(Ok(_ack)) => println!(">>> write ok: plc1.setpoint (unit 1, addr 0) = 4242\n"),
                other => println!(">>> write failed: {other:?}\n"),
            }
        }
    }

    let m = handle.metrics(ch).expect("channel metrics").snapshot();
    println!(
        "final channel metrics: reqs_ok={} reqs_err={} timeouts={} exceptions={} \
         protocol_errors={} reconnects={} writes_ok={} writes_err={}",
        m.reqs_ok, m.reqs_err, m.timeouts, m.exceptions,
        m.protocol_errors, m.reconnects, m.writes_ok, m.writes_err
    );
    println!("total change batches: {batches}");

    handle.shutdown().await; // joins every channel task
    mutator.abort();
    server.abort();
    println!("clean shutdown complete");
}
