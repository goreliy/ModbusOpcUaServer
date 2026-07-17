//! Load tests (plan §9). The sharded scheduler must hold the poll period at
//! scale: every tag accumulates >= 60% of the theoretically possible updates
//! over the run (loopback latency is ~0, so the real figure is ~100%; the
//! margin absorbs CI jitter), coalescing keeps wire-requests ~= device count,
//! and shutdown joins all channel tasks.
//!
//! Ignored by default. Run one:
//!   cargo test -p mb-poller --test load two_hundred_devices -- --ignored --nocapture
//!   cargo test -p mb-poller --test load huge_200k          -- --ignored --nocapture

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mb_poller::{CacheReader, Poller, Quality};
use mb_types::ChannelId;
use tokio::net::TcpListener;

use sim::{spawn_server, Banks};

/// One parameterized load run. `tags_per_device` adjacent holding registers
/// coalesce into a single Modbus transaction (qty must stay <= 125).
async fn run_load(
    channels_n: usize,
    devices_per_channel: usize,
    tags_per_device: usize,
    period_ms: u64,
    run_secs: u64,
) {
    assert!(tags_per_device <= 125, "adjacent regs must fit one PDU");

    // ---- simulators: one listener per channel, N units behind each ----
    let mut servers = Vec::new();
    let mut ports = Vec::new();
    for _ in 0..channels_n {
        let mut units = HashMap::new();
        for unit in 1..=devices_per_channel as u8 {
            let regs: HashMap<u16, u16> =
                (0..tags_per_device as u16).map(|a| (a, a.wrapping_add(100))).collect();
            units.insert(unit, regs);
        }
        let banks: Banks = Arc::new(Mutex::new(units));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        ports.push(listener.local_addr().unwrap().port());
        servers.push(spawn_server(listener, banks));
    }

    // ---- config ----
    let mut channels = Vec::new();
    for (ci, port) in ports.iter().enumerate() {
        let mut devices = Vec::new();
        for di in 0..devices_per_channel {
            let regs: Vec<String> = (0..tags_per_device)
                .map(|ri| {
                    format!(
                        r#"{{ "tag": "c{ci}.d{di}.t{ri}", "poll_group": "fast",
                             "function": "read_holding_registers",
                             "address": {ri}, "data_type": "u16" }}"#
                    )
                })
                .collect();
            devices.push(format!(
                r#"{{ "id": "c{ci}d{di}", "unit_id": {unit}, "registers": [ {regs} ] }}"#,
                unit = di + 1,
                regs = regs.join(",")
            ));
        }
        channels.push(format!(
            r#"{{ "id": "ch{ci}",
                 "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 2000 }},
                 "request_timeout_ms": 1000,
                 "retry": {{ "max_retries": 0, "base_backoff_ms": 100, "max_backoff_ms": 1000 }},
                 "devices": [ {devices} ] }}"#,
            devices = devices.join(",")
        ));
    }
    let json = format!(
        r#"{{ "schema_version": "1",
             "poll_groups": [ {{ "id": "fast", "period_ms": {period_ms} }} ],
             "channels": [ {} ] }}"#,
        channels.join(",")
    );
    let cfg = gateway_config::load_str(&json).expect("load-test config");
    let total_tags = channels_n * devices_per_channel * tags_per_device;
    let total_devices = channels_n * devices_per_channel;
    assert_eq!(cfg.tag_names.len(), total_tags);

    // ---- run ----
    let started = Instant::now();
    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    tokio::time::sleep(Duration::from_secs(run_secs)).await;
    let elapsed = started.elapsed();

    // ---- measure ----
    let snaps = cache.snapshot_all();
    let expected_ticks = (elapsed.as_millis() as u64 / period_ms).max(1);
    let mut min_seq = u64::MAX;
    let mut sum_seq: u64 = 0;
    let mut bad = 0usize;
    for (_, s) in &snaps {
        min_seq = min_seq.min(s.seq);
        sum_seq += s.seq;
        if s.quality != Quality::Good {
            bad += 1;
        }
    }
    let avg_seq = sum_seq as f64 / snaps.len() as f64;

    let mut reqs_ok = 0u64;
    let mut timeouts = 0u64;
    let mut reconnects = 0u64;
    for ci in 0..channels_n {
        let m = handle.metrics(ChannelId(ci as u16)).expect("metrics").snapshot();
        reqs_ok += m.reqs_ok;
        timeouts += m.timeouts;
        reconnects += m.reconnects;
    }

    println!("== LOAD REPORT ==");
    println!(
        "scale: {channels_n} channels x {devices_per_channel} devices x {tags_per_device} tags = {total_tags} tags / {total_devices} devices"
    );
    println!(
        "period: {period_ms} ms, run: {:.1}s -> expected ~{expected_ticks} ticks",
        elapsed.as_secs_f64()
    );
    println!("per-tag updates: min={min_seq} avg={avg_seq:.1} (of ~{expected_ticks} possible)");
    println!(
        "requests ok: {reqs_ok} (coalescing: ~{:.0} tags/request), timeouts: {timeouts}, reconnects: {reconnects}, non-Good tags: {bad}",
        total_tags as f64 / (reqs_ok as f64 / expected_ticks as f64).max(1.0)
    );

    // ---- assertions ----
    assert_eq!(bad, 0, "every tag must be Good at scale");
    assert_eq!(timeouts, 0, "loopback must not time out");
    assert_eq!(reconnects, 0, "no reconnect churn");
    assert!(
        (min_seq as f64) >= expected_ticks as f64 * 0.6,
        "slowest tag got {min_seq} updates of ~{expected_ticks} expected: period not held"
    );
    // Coalescing: adjacent tags of a device -> 1 request, so requests/tick
    // tracks device count, not tag count.
    let reqs_per_tick = reqs_ok as f64 / expected_ticks as f64;
    assert!(
        reqs_per_tick < (total_devices * 2) as f64,
        "requests/tick {reqs_per_tick:.0} indicates coalescing is not effective"
    );

    let shut_started = Instant::now();
    tokio::time::timeout(Duration::from_secs(20), handle.shutdown())
        .await
        .expect("shutdown joins all channel tasks");
    println!("shutdown of {channels_n} channels took {:?}", shut_started.elapsed());
    for s in servers {
        s.abort();
    }
}

/// 20 channels x 10 devices x 25 tags = 5 000 tags / 200 devices, 200 ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "load test, ~10s; run explicitly with --ignored"]
async fn two_hundred_devices_five_thousand_tags_hold_the_period() {
    run_load(20, 10, 25, 200, 10).await;
}

/// 40 channels x 50 devices x 100 tags = 200 000 tags / 2 000 devices, 1 s.
/// The big one: proves the flat cache, coalescer and per-channel scheduler
/// hold at 200k tags on loopback.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "heavy load test, ~15s + build; run explicitly with --ignored"]
async fn huge_200k_tags_hold_the_period() {
    run_load(40, 50, 100, 1000, 12).await;
}
