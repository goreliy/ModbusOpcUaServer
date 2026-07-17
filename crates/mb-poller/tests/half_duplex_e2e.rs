//! Half-duplex (RTU-over-TCP) end-to-end regression tests for the review
//! findings #1/#2/#3 and #27:
//!
//! (a) a permanently dead slave on a half-duplex bus crosses the offline
//!     watchdog (tags Bad) even though its timeouts force stream-drain
//!     reconnects — `DeviceRuntime` state survives the reconnect;
//! (b) healthy devices on the same bus keep polling Good (no starvation, no
//!     reconnect storm) while the dead slave is probe-gated;
//! (c) failed probes of the offline device never upgrade its quality from
//!     Bad back to Uncertain;
//! (d) writes queued while the channel cannot connect fail fast with
//!     `NotConnected` instead of blocking for the outage.

#[path = "../testsupport/sim.rs"]
mod sim;

use std::collections::HashMap;
use std::future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mb_poller::{Poller, Quality, WriteCommand};
use mb_proto::{ModbusRequest, ProtoError};
use mb_types::{ChannelId, DeviceId};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::rtu_over_tcp::{accept_tcp_connection, Server},
    SlaveRequest,
};

use sim::{snap, wait_for, Banks};

/// RTU-framed multi-slave sim behind one "terminal server" socket. Units with
/// a register bank answer; units WITHOUT a bank never answer at all (a dead
/// RS-485 slave: the client sees pure request timeouts, not exceptions).
#[derive(Clone)]
struct RtuMultiSlave {
    banks: Banks,
}

impl tokio_modbus::server::Service for RtuMultiSlave {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let SlaveRequest { slave, request } = req;
        let mut banks = self.banks.lock().unwrap();
        let Some(bank) = banks.get_mut(&slave) else {
            return future::ready(Ok(None)); // dead slave: silence
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

fn spawn_rtu_server(listener: TcpListener, banks: Banks) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let server = Server::new(listener);
        let service = RtuMultiSlave { banks };
        let on_connected = move |stream, socket_addr| {
            let service = service.clone();
            async move {
                accept_tcp_connection(stream, socket_addr, move |_| Ok(Some(service.clone())))
            }
        };
        let _ = server
            .serve(&on_connected, |err| eprintln!("rtu server error: {err}"))
            .await;
    })
}

/// #1/#2/#3: dead slave on a half-duplex bus — offline watchdog trips (Bad),
/// probes stay single-shot and never lift quality back to Uncertain, and the
/// healthy slave on the same bus keeps updating Good without reconnect churn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_slave_goes_bad_and_healthy_slave_survives_on_half_duplex() {
    // Unit 1 answers; unit 2 has no bank -> never answers (pure timeouts).
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0, 77)]),
    )])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = spawn_rtu_server(listener, Arc::clone(&banks));

    // Dead device has TWO non-coalescable transactions (max_gap 0, addresses
    // 0 and 10): back-to-back timeouts trigger the §7 stream-drain reconnect
    // every time it is polled while still counting toward offline_after=2 —
    // exactly the livelock scenario of finding #1.
    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [ {{ "id": "fast", "period_ms": 150 }} ],
        "channels": [ {{
            "id": "bus",
            "transport": {{ "type": "rtu_over_tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 100,
            "inter_request_delay_ms": 1,
            "offline_after_failures": 2,
            "max_gap": 0,
            "retry": {{ "max_retries": 1, "base_backoff_ms": 100, "max_backoff_ms": 400 }},
            "devices": [
                {{ "id": "alive", "unit_id": 1, "registers": [
                    {{ "tag": "a.0", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }}
                ] }},
                {{ "id": "dead", "unit_id": 2, "registers": [
                    {{ "tag": "d.0", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }},
                    {{ "tag": "d.1", "poll_group": "fast", "function": "read_holding_registers", "address": 10, "data_type": "u16" }}
                ] }}
            ]
        }} ]
    }}"#
    ))
    .expect("config loads");

    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    let metrics = handle.metrics(ChannelId(0)).expect("channel metrics");

    // The healthy slave reaches Good despite sharing the bus with a dead one.
    wait_for("alive tag Good", Duration::from_secs(5), || {
        snap(&cache, "a.0").quality == Quality::Good
    })
    .await;

    // #1/#2: the offline watchdog is reachable on half-duplex — the dead
    // slave's failure counter survives the stream-drain reconnects and its
    // tags are swept Bad at the offline_after=2 crossing. (Pre-fix this never
    // happened: on_connected() wiped `fails` after every consec-timeout
    // reconnect.) Tags START Bad (never-read), so anchor on the timeouts
    // metric: >= 2 timeouts means the counter really crossed the threshold
    // (the first timeout degrades d.0 Uncertain while still online).
    wait_for("dead device crosses offline threshold", Duration::from_secs(5), || {
        metrics.snapshot().timeouts >= 2
            && snap(&cache, "d.0").quality == Quality::Bad
            && snap(&cache, "d.1").quality == Quality::Bad
    })
    .await;

    // Steady-state window: probes of the offline device must not lift quality
    // back to Uncertain (#3), the healthy device must keep updating (#1
    // starvation), and reconnects must stay bounded (no churn).
    let reconnects_at_bad = metrics.snapshot().reconnects;
    let alive_seq0 = snap(&cache, "a.0").seq;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            snap(&cache, "d.0").quality,
            Quality::Bad,
            "failed probe must never upgrade an offline device's quality (#3)"
        );
        assert_eq!(snap(&cache, "a.0").quality, Quality::Good, "healthy slave starved (#1)");
    }
    let alive_delta = snap(&cache, "a.0").seq - alive_seq0;
    assert!(
        alive_delta >= 2,
        "healthy device must keep polling while the dead one is gated: seq delta {alive_delta}"
    );
    let reconnect_churn = metrics.snapshot().reconnects - reconnects_at_bad;
    assert!(
        reconnect_churn <= 2,
        "probe-gated dead slave must not cycle the connection: {reconnect_churn} reconnects in 600ms"
    );

    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown joins channel tasks");
    server.abort();
}

/// B1: a write whose deadline has already passed when the channel task
/// dequeues it is dropped WITHOUT touching the device — the submitter gets
/// `Timeout` and the device register keeps its value. A fresh write on the
/// same healthy channel still goes through (the drop is per-command, not a
/// channel state).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_write_is_dropped_without_touching_the_device() {
    let banks: Banks = Arc::new(Mutex::new(HashMap::from([(
        1,
        HashMap::from([(0u16, 77u16)]),
    )])));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = sim::spawn_server(listener, Arc::clone(&banks));

    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [ {{ "id": "fast", "period_ms": 100 }} ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 1000 }},
            "request_timeout_ms": 250,
            "devices": [ {{ "id": "dev1", "unit_id": 1, "registers": [
                {{ "tag": "t.0", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }}
            ] }} ]
        }} ]
    }}"#
    ))
    .expect("config loads");

    let (handle, cache) = Poller::spawn_with_cache(&cfg);
    let writer = handle.writer(ChannelId(0)).expect("writer");

    // Channel connected and polling (so the write path is live, not the
    // fail-fast-while-disconnected path).
    wait_for("initial poll Good", Duration::from_secs(5), || {
        sim::good_u16(&cache, "t.0", 77)
    })
    .await;

    // Deadline already in the past: dropped, device untouched.
    let (tx, rx) = oneshot::channel();
    writer
        .send(WriteCommand {
            device: DeviceId(0),
            req: ModbusRequest::WriteSingleRegister { addr: 0, value: 9999 },
            reply: tx,
            deadline: Instant::now() - Duration::from_millis(1),
        })
        .await
        .expect("send expired write");
    let res = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("expired write answered promptly")
        .expect("reply oneshot alive");
    assert!(
        matches!(res, Err(ProtoError::Timeout)),
        "expired write must reply Timeout, got {res:?}"
    );
    assert_eq!(
        banks.lock().unwrap()[&1][&0],
        77,
        "expired write must never reach the device"
    );

    // A fresh write with a generous deadline still lands.
    let (tx, rx) = oneshot::channel();
    writer
        .send(WriteCommand {
            device: DeviceId(0),
            req: ModbusRequest::WriteSingleRegister { addr: 0, value: 4242 },
            reply: tx,
            deadline: Instant::now() + Duration::from_secs(60),
        })
        .await
        .expect("send valid write");
    let res = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("valid write answered")
        .expect("reply oneshot alive");
    assert!(res.is_ok(), "valid write must be acked, got {res:?}");
    assert_eq!(banks.lock().unwrap()[&1][&0], 4242);

    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown joins channel tasks");
    server.abort();
}

/// #27: writes submitted while the channel cannot connect are failed fast
/// with `NotConnected` — both those already queued and those arriving during
/// the reconnect backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_fail_fast_while_disconnected() {
    // Grab a free port and close it again: connects will be refused.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let cfg = gateway_config::load_str(&format!(
        r#"{{
        "schema_version": "1",
        "poll_groups": [ {{ "id": "fast", "period_ms": 100 }} ],
        "channels": [ {{
            "id": "plc",
            "transport": {{ "type": "tcp", "host": "127.0.0.1", "port": {port}, "connect_timeout_ms": 250 }},
            "request_timeout_ms": 250,
            "retry": {{ "max_retries": 0, "base_backoff_ms": 50, "max_backoff_ms": 200 }},
            "devices": [ {{ "id": "dev1", "unit_id": 1, "registers": [
                {{ "tag": "t.0", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" }}
            ] }} ]
        }} ]
    }}"#
    ))
    .expect("config loads");

    let (handle, _cache) = Poller::spawn_with_cache(&cfg);
    let writer = handle.writer(ChannelId(0)).expect("writer");

    let mut replies = Vec::new();
    for value in 0..3u16 {
        let (tx, rx) = oneshot::channel();
        writer
            .send(WriteCommand {
                device: DeviceId(0),
                req: ModbusRequest::WriteSingleRegister { addr: 0, value },
                reply: tx,
                deadline: Instant::now() + Duration::from_secs(60),
            })
            .await
            .expect("send write");
        replies.push(rx);
    }

    for rx in replies {
        let res = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("write must be answered promptly while disconnected (#27)")
            .expect("reply oneshot alive");
        assert!(
            matches!(res, Err(ProtoError::NotConnected)),
            "queued write during outage must fail NotConnected, got {res:?}"
        );
    }

    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown joins channel tasks");
}
