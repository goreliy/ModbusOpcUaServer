//! `run_channel` (design §4): the one owning task per physical channel.
//!
//! Connect / jittered-exponential-backoff reconnect loop, whole-channel Bad
//! sweep on connect failure, `select!` over shutdown / writes / due polls,
//! per-request retry, the §7 error->quality mapping (incl. the
//! second-consecutive-timeout-is-fatal stream-drain rule on half-duplex
//! transports), the per-device offline watchdog and adaptive de-coalescing.
//!
//! Half-duplex policy (review findings #1-#5/#7/#8/#12/#13/#18/#19/#26/#27):
//! - `DeviceRuntime` state SURVIVES reconnects; failure counters reset only on
//!   a successful request to that device, so the offline watchdog is reachable
//!   even when the consec-timeout rule cycles the connection.
//! - On half-duplex transports a `Timeout` is never retried on the same
//!   connection (a late reply could alias the retry); transient exceptions
//!   still retry.
//! - Probes of offline devices are single-shot (no retry budget).
//! - An exception reply is proof of life: it revives the device before the
//!   exception is classified.
//! - Reconnect backoff resets to base only after the first successful request
//!   on the new connection, not on connect success.
//! - Writes participate in the stream-drain rule; any write timeout on a
//!   half-duplex transport forces a reconnect after the submitter is answered.
//! - Pending writes are drained at every transaction boundary inside a tick;
//!   while disconnected they fail fast with `NotConnected`.
//!
//! TODO(phase1-followup): `max_inflight > 1` on TCP — N independent
//! `Context`s (one socket per worker) behind a `Semaphore` +
//! `FuturesUnordered` per design §4. This stage ships sequential
//! (`max_inflight = 1`) semantics for ALL transports; validation WARNS when a
//! TCP channel configures more (`MaxInflightUnsupported`) and resolve forces
//! the value to 1, but the schema field and the plan slot stay so the upgrade
//! is local to this module.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{mpsc, watch};

use gateway_config::schema::v1::RetryConfig;
use mb_proto::{ExceptionCode, ModbusRequest, ModbusResponse, ProtoError, Transport};
use mb_types::TagId;

use crate::cache::{Quality, RawValue, RawValueSink};
use crate::command::WriteCommand;
use crate::device::DeviceRuntime;
use crate::metrics::ChannelMetrics;
use crate::plan::{ChannelPlan, Transaction};
use crate::schedule::PollWheel;

/// What a poll tick asks the connection loop to do next.
enum TickOutcome {
    Continue,
    /// Fatal error: drop the connection, back off, reconnect.
    Reconnect,
}

/// What servicing one write command asks the connection loop to do next.
enum WriteOutcome {
    Continue,
    /// Fatal error OR a write timeout on a half-duplex transport (#5/#13):
    /// drop the connection, back off, reconnect.
    Reconnect,
}

pub async fn run_channel(
    plan: ChannelPlan,
    sink: Arc<dyn RawValueSink>,
    mut writes: mpsc::Receiver<WriteCommand>,
    mut shutdown: watch::Receiver<bool>,
    metrics: Arc<ChannelMetrics>,
) {
    let mut rt: Vec<DeviceRuntime> = plan
        .devices
        .iter()
        .map(|d| DeviceRuntime::new(d.offline_after, d.retry))
        .collect();
    // #14: writes are keyed by DeviceId; resolve to the plan's (enabled-
    // filtered) index here, once.
    let dev_index: std::collections::HashMap<mb_types::DeviceId, usize> = plan
        .devices
        .iter()
        .enumerate()
        .map(|(i, d)| (d.id, i))
        .collect();
    let mut wheel = PollWheel::new(&plan);
    let mut backoff_ms = plan.retry.base_backoff_ms;
    // #28: edge-detect the whole-channel Bad sweep — during an outage the
    // early backoff rounds must not flood the change ring with no-op batches
    // (they'd evict live batches from healthy channels and force subscribers
    // into full-resync).
    let mut swept = false;

    'reconnect: loop {
        if *shutdown.borrow() {
            return;
        }

        // ---- connect / backoff / whole-channel watchdog ----
        let traffic = plan.log_traffic.then_some(plan.name.as_str());
        let mut tx = match Transport::connect_traced(&plan.transport, traffic).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(channel = plan.id.0, error = %e, "channel connect failed");
                ChannelMetrics::bump(&metrics.reconnects);
                // Whole channel down: mark every tag of every device Bad AND
                // notify subscribers (set_device_quality emits a batch) —
                // once per disconnected period, not per backoff attempt.
                if !swept {
                    swept = true;
                    for d in &plan.devices {
                        sink.set_device_quality(&d.all_tags, Quality::Bad);
                    }
                }
                // #27: don't let stale writes wait out the outage.
                fail_pending_writes(&mut writes, &metrics);
                if backoff_or_stop(&mut backoff_ms, &plan.retry, &mut shutdown, &mut writes, &metrics).await {
                    return;
                }
                continue 'reconnect;
            }
        };
        tracing::info!(channel = plan.id.0, "channel connected");
        swept = false;
        // #1/#2: DeviceRuntime state (fails / online / probe backoff)
        // deliberately survives the reconnect: a fresh connection says nothing
        // about a slave that just timed out. Counters reset per device on its
        // first successful request (`on_success`).
        //
        // #7: `backoff_ms` is NOT reset here. A serial open / terminal-server
        // accept succeeds even on a dead bus; backoff resets to base only once
        // this connection carries a successful request (`healthy`).
        let half_duplex = tx.is_half_duplex();
        let mut consec_timeouts = 0u32;
        let mut healthy = false;

        // ---- poll + write loop (sequential; one request in flight) ----
        loop {
            tokio::select! {
                biased;

                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        let _ = tx.disconnect().await;
                        return;
                    }
                }

                // Writes get priority: serviced at a select! boundary or a
                // transaction boundary inside run_tick (#8), never
                // mid-coalesced-burst -> no RS-485 frame collision.
                Some(cmd) = writes.recv() => {
                    let outcome = service_write(
                        cmd, &plan, &dev_index, &mut tx, &mut rt, &*sink, &metrics,
                        &mut consec_timeouts, half_duplex, &mut healthy,
                    ).await;
                    match outcome {
                        WriteOutcome::Continue => inter_request_gap(&plan, half_duplex).await,
                        WriteOutcome::Reconnect => {
                            ChannelMetrics::bump(&metrics.reconnects);
                            if healthy {
                                backoff_ms = plan.retry.base_backoff_ms;
                            }
                            fail_pending_writes(&mut writes, &metrics);
                            if backoff_or_stop(&mut backoff_ms, &plan.retry, &mut shutdown, &mut writes, &metrics).await {
                                return;
                            }
                            continue 'reconnect;
                        }
                    }
                }

                due = wheel.next_due() => {
                    let outcome = run_tick(
                        due, &plan, &dev_index, &mut tx, &mut rt, &*sink, &metrics,
                        &mut writes, &shutdown, &mut consec_timeouts, half_duplex, &mut healthy,
                    ).await;
                    if matches!(outcome, TickOutcome::Reconnect) {
                        ChannelMetrics::bump(&metrics.reconnects);
                        if healthy {
                            backoff_ms = plan.retry.base_backoff_ms;
                        }
                        fail_pending_writes(&mut writes, &metrics);
                        if backoff_or_stop(&mut backoff_ms, &plan.retry, &mut shutdown, &mut writes, &metrics).await {
                            return;
                        }
                        continue 'reconnect;
                    }
                }
            }
        }
    }
}

/// One scheduler batch: poll every due `(device, group)` sequentially,
/// draining queued writes at every transaction boundary (#8) so operator
/// write latency is bounded by ~one transaction, not one whole batch.
#[allow(clippy::too_many_arguments)]
async fn run_tick(
    due: Vec<(usize, usize)>,
    plan: &ChannelPlan,
    dev_index: &std::collections::HashMap<mb_types::DeviceId, usize>,
    tx: &mut Transport,
    rt: &mut [DeviceRuntime],
    sink: &dyn RawValueSink,
    metrics: &ChannelMetrics,
    writes: &mut mpsc::Receiver<WriteCommand>,
    shutdown: &watch::Receiver<bool>,
    consec_timeouts: &mut u32,
    half_duplex: bool,
    healthy: &mut bool,
) -> TickOutcome {
    'devices: for (dev_idx, group_idx) in due {
        // #11: a batch at the >100-device scale can run for minutes when many
        // devices are timing out — observe shutdown between transactions
        // (never mid-request: the half-duplex invariant holds, at worst one
        // request_timeout of extra latency).
        if *shutdown.borrow() {
            return TickOutcome::Continue;
        }
        // Don't burn a timeout on a dead slave except on its probe cadence.
        if rt[dev_idx].is_offline_and_not_due_to_probe() {
            continue;
        }
        let dev = &plan.devices[dev_idx];
        let base_txns = &dev.by_group[group_idx].1;

        for (txn_idx, base_txn) in base_txns.iter().enumerate() {
            // Adaptive de-coalescing (§5): poll the remembered split instead.
            let split = rt[dev_idx].effective_txns(group_idx, txn_idx);
            let txns: &[Transaction] = match &split {
                Some(subs) => subs,
                None => std::slice::from_ref(base_txn),
            };

            for txn in txns {
                // #11: shutdown is observed at every transaction boundary.
                if *shutdown.borrow() {
                    return TickOutcome::Continue;
                }
                // #18: probes of offline devices are single-shot — the device
                // already failed `offline_after` requests in a row; extra
                // retries only steal bus time from healthy slaves.
                // #9/#15/#31: retry budget and request timeout are the
                // device-resolved values, not the channel's.
                let budget = retry_budget(rt[dev_idx].is_online(), &dev.retry);
                match poll_with_retry(tx, dev, &txn.req, plan, metrics, half_duplex, budget)
                    .await
                {
                    Ok(resp) => {
                        *consec_timeouts = 0;
                        *healthy = true;
                        if rt[dev_idx].on_success() {
                            tracing::info!(device = %dev.id.0, "device back online");
                        }
                        scatter(sink, txn, resp);
                    }
                    Err(e) if e.is_fatal() => {
                        tracing::warn!(device = %dev.id.0, error = %e, "fatal comm error");
                        if rt[dev_idx].on_comm_failure() {
                            sink.set_device_quality(&dev.all_tags, Quality::Bad);
                        }
                        return TickOutcome::Reconnect; // drop conn, reconnect w/ backoff
                    }
                    Err(ProtoError::Exception(exc)) => {
                        // Device alive (it answered): link healthy, frame in sync.
                        *consec_timeouts = 0;
                        *healthy = true;
                        // #19: an exception reply is proof of life — revive the
                        // device (reset fails, clear probe backoff) BEFORE the
                        // exception is classified, so probe gating can re-arm
                        // and "back online" fires.
                        if rt[dev_idx].on_success() {
                            tracing::info!(device = %dev.id.0, "device back online (exception reply)");
                        }
                        rt[dev_idx].handle_exception(group_idx, txn_idx, txn, exc, sink);
                    }
                    Err(_) => {
                        // Timeout: device may be flaky, not yet offline.
                        *consec_timeouts += 1;
                        if rt[dev_idx].on_comm_failure() {
                            sink.set_device_quality(&dev.all_tags, Quality::Bad);
                        } else if rt[dev_idx].is_online() {
                            sink.degrade_uncertain(&txn.tags()); // stale-but-maybe-recovering
                        }
                        // #3: a failed probe of an already-offline device takes
                        // neither branch — quality stays Bad.

                        // §7 stream-drain rule: a dropped in-flight exchange can
                        // desync a byte stream; a second consecutive timeout
                        // forces a clean reconnect instead of trusting resync.
                        if half_duplex && *consec_timeouts >= 2 {
                            tracing::warn!(
                                device = %dev.id.0,
                                "second consecutive timeout on stream transport: reconnecting"
                            );
                            return TickOutcome::Reconnect;
                        }
                        if !rt[dev_idx].is_online() {
                            // Failed probe: skip the rest of this device's
                            // transactions for this tick — but still honor the
                            // silent gap (#26: a late reply may be arriving)
                            // and the write drain at this boundary.
                            inter_request_gap(plan, half_duplex).await;
                            if matches!(
                                drain_pending_writes(
                                    plan, dev_index, tx, rt, sink, metrics, writes,
                                    consec_timeouts, half_duplex, healthy,
                                )
                                .await,
                                WriteOutcome::Reconnect
                            ) {
                                return TickOutcome::Reconnect;
                            }
                            continue 'devices;
                        }
                    }
                }
                // RS-485 turnaround / silent gap between transactions (t3.5).
                // #26: after EVERY sent request, including the timeout branch.
                inter_request_gap(plan, half_duplex).await;
                // #8: transaction boundary — service queued operator writes now.
                if matches!(
                    drain_pending_writes(
                        plan, dev_index, tx, rt, sink, metrics, writes,
                        consec_timeouts, half_duplex, healthy,
                    )
                    .await,
                    WriteOutcome::Reconnect
                ) {
                    return TickOutcome::Reconnect;
                }
            }
        }
    }
    TickOutcome::Continue
}

/// Per-request retry budget: probes of offline devices are single-shot (#18).
fn retry_budget(online: bool, retry: &RetryConfig) -> u32 {
    if online {
        retry.max_retries
    } else {
        0
    }
}

/// Whether an error may be retried on the SAME connection. #4/#12: a `Timeout`
/// is never retried on half-duplex — the late reply to the timed-out request
/// could be consumed as the retry's answer (tokio-modbus correlates by slave
/// id + function code only), silently shifting the stream by one frame.
fn is_retryable(e: &ProtoError, half_duplex: bool) -> bool {
    match e {
        ProtoError::Timeout => !half_duplex,
        ProtoError::Exception(
            ExceptionCode::ServerDeviceBusy | ExceptionCode::GatewayTargetDevice,
        ) => true,
        _ => false,
    }
}

/// RS-485 turnaround / silent gap (t3.5) after every sent request on
/// half-duplex (#26); no-op on TCP or with a zero delay.
async fn inter_request_gap(plan: &ChannelPlan, half_duplex: bool) {
    if half_duplex && !plan.inter_request_delay.is_zero() {
        tokio::time::sleep(plan.inter_request_delay).await;
    }
}

/// One transaction with per-request retry on the SAME connection (§4):
/// `Timeout` (full-duplex only, see [`is_retryable`]) and transient exceptions
/// (`ServerDeviceBusy`, `GatewayTargetDevice`) retry up to `max_retries`;
/// deterministic exceptions (`Illegal*`) and fatal errors return immediately.
/// Timeout budget is the device-resolved value (#9/#15/#31).
#[allow(clippy::too_many_arguments)]
async fn poll_with_retry(
    tx: &mut Transport,
    dev: &crate::plan::DevicePlan,
    req: &ModbusRequest,
    plan: &ChannelPlan,
    metrics: &ChannelMetrics,
    half_duplex: bool,
    max_retries: u32,
) -> Result<ModbusResponse, ProtoError> {
    let mut attempt = 0u32;
    loop {
        let res = tx.request(dev.unit, req, dev.request_timeout).await;
        match &res {
            Ok(_) => {
                ChannelMetrics::bump(&metrics.reqs_ok);
                return res;
            }
            Err(e) => {
                ChannelMetrics::bump(&metrics.reqs_err);
                match e {
                    ProtoError::Timeout => ChannelMetrics::bump(&metrics.timeouts),
                    ProtoError::Exception(_) => ChannelMetrics::bump(&metrics.exceptions),
                    ProtoError::Protocol { .. } => ChannelMetrics::bump(&metrics.protocol_errors),
                    _ => {}
                }
                if e.is_fatal() {
                    return res;
                }
                if !is_retryable(e, half_duplex) || attempt >= max_retries {
                    return res;
                }
                attempt += 1;
                inter_request_gap(plan, half_duplex).await;
            }
        }
    }
}

/// Drain queued writes non-blockingly at a transaction boundary (#8). Each
/// serviced write is followed by the half-duplex silent gap.
#[allow(clippy::too_many_arguments)]
async fn drain_pending_writes(
    plan: &ChannelPlan,
    dev_index: &std::collections::HashMap<mb_types::DeviceId, usize>,
    tx: &mut Transport,
    rt: &mut [DeviceRuntime],
    sink: &dyn RawValueSink,
    metrics: &ChannelMetrics,
    writes: &mut mpsc::Receiver<WriteCommand>,
    consec_timeouts: &mut u32,
    half_duplex: bool,
    healthy: &mut bool,
) -> WriteOutcome {
    while let Ok(cmd) = writes.try_recv() {
        match service_write(
            cmd, plan, dev_index, tx, rt, sink, metrics, consec_timeouts, half_duplex, healthy,
        )
        .await
        {
            WriteOutcome::Reconnect => return WriteOutcome::Reconnect,
            WriteOutcome::Continue => inter_request_gap(plan, half_duplex).await,
        }
    }
    WriteOutcome::Continue
}

/// Service one write command. The submitter always gets its `oneshot` reply
/// first; only then does the caller act on the returned [`WriteOutcome`].
///
/// Writes participate in the §7 stream-drain rule (#5/#13): a timeout counts
/// toward `consec_timeouts` / the timeout metric / the device watchdog, and on
/// a half-duplex transport ANY write timeout forces a reconnect — a late FC06
/// echo would otherwise be accepted as the next exchange's reply (tokio-modbus
/// checks the echoed addr/value with `debug_assert!` only).
#[allow(clippy::too_many_arguments)]
async fn service_write(
    cmd: WriteCommand,
    plan: &ChannelPlan,
    dev_index: &std::collections::HashMap<mb_types::DeviceId, usize>,
    tx: &mut Transport,
    rt: &mut [DeviceRuntime],
    sink: &dyn RawValueSink,
    metrics: &ChannelMetrics,
    consec_timeouts: &mut u32,
    half_duplex: bool,
    healthy: &mut bool,
) -> WriteOutcome {
    // B1: an expired command is dropped WITHOUT touching the device — the
    // submitter (e.g. the OPC UA write callback) stopped waiting at this same
    // instant and its client already got a timeout; executing late would
    // double-fire the actuator.
    if Instant::now() > cmd.deadline {
        tracing::warn!(device = cmd.device.0, "expired write dropped");
        let _ = cmd.reply.send(Err(ProtoError::Timeout));
        ChannelMetrics::bump(&metrics.writes_err);
        return WriteOutcome::Continue;
    }
    // #14: resolve the write target by identity; an unknown or disabled
    // device gets an explicit error instead of a positional-index gamble.
    let Some(&idx) = dev_index.get(&cmd.device) else {
        let _ = cmd
            .reply
            .send(Err(ProtoError::unexpected_response(format!(
                "write to unknown or disabled device {}",
                cmd.device.0
            ))));
        ChannelMetrics::bump(&metrics.writes_err);
        return WriteOutcome::Continue;
    };
    let dev = &plan.devices[idx];
    // #9/#15/#31: writes honor the device-resolved timeout too.
    let res = tx.request(dev.unit, &cmd.req, dev.request_timeout).await;
    let outcome = match &res {
        Ok(_) => {
            ChannelMetrics::bump(&metrics.reqs_ok);
            ChannelMetrics::bump(&metrics.writes_ok);
            *consec_timeouts = 0;
            *healthy = true;
            if rt[idx].on_success() {
                tracing::info!(device = %dev.id.0, "device back online (write ack)");
            }
            WriteOutcome::Continue
        }
        Err(ProtoError::Exception(_)) => {
            ChannelMetrics::bump(&metrics.reqs_err);
            ChannelMetrics::bump(&metrics.writes_err);
            ChannelMetrics::bump(&metrics.exceptions);
            *consec_timeouts = 0;
            *healthy = true;
            // #19 (write side): an exception reply is still proof of life.
            if rt[idx].on_success() {
                tracing::info!(device = %dev.id.0, "device back online (exception reply)");
            }
            WriteOutcome::Continue
        }
        Err(ProtoError::Timeout) => {
            ChannelMetrics::bump(&metrics.reqs_err);
            ChannelMetrics::bump(&metrics.writes_err);
            ChannelMetrics::bump(&metrics.timeouts);
            *consec_timeouts += 1;
            if rt[idx].on_comm_failure() {
                sink.set_device_quality(&dev.all_tags, Quality::Bad);
            }
            if half_duplex {
                tracing::warn!(
                    device = %dev.id.0,
                    "write timeout on stream transport: reconnecting"
                );
                WriteOutcome::Reconnect
            } else {
                WriteOutcome::Continue
            }
        }
        Err(e) => {
            debug_assert!(e.is_fatal(), "non-fatal errors are handled above");
            ChannelMetrics::bump(&metrics.reqs_err);
            ChannelMetrics::bump(&metrics.writes_err);
            if matches!(e, ProtoError::Protocol { .. }) {
                ChannelMetrics::bump(&metrics.protocol_errors);
            }
            if rt[idx].on_comm_failure() {
                sink.set_device_quality(&dev.all_tags, Quality::Bad);
            }
            WriteOutcome::Reconnect
        }
    };
    let _ = cmd.reply.send(res);
    outcome
}

/// Fail every already-queued write with `NotConnected` (#27): during an
/// outage submitters get an immediate error instead of blocking unboundedly —
/// and no stale command fires at the plant after recovery.
fn fail_pending_writes(writes: &mut mpsc::Receiver<WriteCommand>, metrics: &ChannelMetrics) {
    while let Ok(cmd) = writes.try_recv() {
        let _ = cmd.reply.send(Err(ProtoError::NotConnected));
        ChannelMetrics::bump(&metrics.writes_err);
    }
}

/// Slice one response into per-field cache writes (one `publish_batch` per
/// transaction). Fields that fall outside a short response — or a Custom
/// reply whose length contradicts `expect_len` — degrade Bad instead.
///
/// #6: the TOTAL payload length is validated against the request `qty` first.
/// A wrong-size reply is by Modbus contract malformed (and in practice a stale
/// frame from an earlier exchange), so ALL of the transaction's tags degrade
/// Bad — in-range fields must not be published Good from a reply we cannot
/// trust. Defense-in-depth behind the same check in `Transport::request`.
fn scatter(sink: &dyn RawValueSink, txn: &Transaction, resp: ModbusResponse) {
    let ts = SystemTime::now();
    let mono = Instant::now();
    let mut updates: Vec<(TagId, RawValue)> = Vec::with_capacity(txn.fields.len());
    let mut bad: Vec<TagId> = Vec::new();

    let expected_qty = match &txn.req {
        ModbusRequest::ReadCoils { qty, .. }
        | ModbusRequest::ReadDiscreteInputs { qty, .. }
        | ModbusRequest::ReadHoldingRegisters { qty, .. }
        | ModbusRequest::ReadInputRegisters { qty, .. } => Some(*qty as usize),
        _ => None,
    };

    match resp {
        ModbusResponse::Registers(regs) => {
            if expected_qty.is_some_and(|q| regs.len() != q) {
                tracing::warn!(
                    got = regs.len(),
                    expected = expected_qty.unwrap_or(0),
                    "register response length mismatch: all txn tags degraded Bad"
                );
                bad.extend(txn.fields.iter().map(|f| f.tag));
            } else {
                for f in &txn.fields {
                    let start = f.word_offset as usize;
                    match regs.get(start..start + f.word_len as usize) {
                        Some(words) => updates.push((f.tag, RawValue::Registers(Arc::from(words)))),
                        None => bad.push(f.tag),
                    }
                }
            }
        }
        ModbusResponse::Bits(bits) => {
            // Bit responses are byte-padded on the wire; the transport already
            // truncates to qty, so `<` means a short (malformed) reply.
            if expected_qty.is_some_and(|q| bits.len() < q) {
                tracing::warn!(
                    got = bits.len(),
                    expected = expected_qty.unwrap_or(0),
                    "bit response shorter than requested: all txn tags degraded Bad"
                );
                bad.extend(txn.fields.iter().map(|f| f.tag));
            } else {
                for f in &txn.fields {
                    let start = f.word_offset as usize;
                    match bits.get(start..start + f.word_len as usize) {
                        Some(b) => updates.push((f.tag, RawValue::Bits(Arc::from(b)))),
                        None => bad.push(f.tag),
                    }
                }
            }
        }
        ModbusResponse::Raw(bytes) => {
            let expected = match &txn.req {
                ModbusRequest::Custom { expect_len, .. } => *expect_len,
                _ => None,
            };
            if expected.is_some_and(|n| bytes.len() != n as usize) {
                tracing::warn!(
                    got = bytes.len(),
                    expected = expected.unwrap_or(0),
                    "custom response length mismatch"
                );
                bad.extend(txn.fields.iter().map(|f| f.tag));
            } else {
                for f in &txn.fields {
                    updates.push((f.tag, RawValue::Raw(bytes.clone())));
                }
            }
        }
        ModbusResponse::WriteAck => return,
    }

    if !updates.is_empty() {
        sink.publish_batch(&updates, ts, mono);
    }
    if !bad.is_empty() {
        tracing::warn!(tags = bad.len(), "response shorter than plan: tags degraded Bad");
        sink.set_device_quality(&bad, Quality::Bad);
    }
}

/// Jittered exponential reconnect backoff; `true` = shutdown requested.
/// Writes arriving during the backoff fail fast with `NotConnected` (#27)
/// instead of queueing up for the whole outage.
async fn backoff_or_stop(
    backoff_ms: &mut u64,
    retry: &RetryConfig,
    shutdown: &mut watch::Receiver<bool>,
    writes: &mut mpsc::Receiver<WriteCommand>,
    metrics: &ChannelMetrics,
) -> bool {
    let delay = jitter(*backoff_ms);
    *backoff_ms = backoff_ms
        .saturating_mul(2)
        .min(retry.max_backoff_ms)
        .max(1);
    let sleep = tokio::time::sleep(Duration::from_millis(delay));
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return false,
            res = shutdown.changed() => return res.is_err() || *shutdown.borrow(),
            cmd = writes.recv() => match cmd {
                Some(cmd) => {
                    let _ = cmd.reply.send(Err(ProtoError::NotConnected));
                    ChannelMetrics::bump(&metrics.writes_err);
                }
                // All write senders dropped: stop selecting on the closed
                // channel (recv() would resolve immediately forever).
                None => break,
            },
        }
    }
    tokio::select! {
        _ = &mut sleep => false,
        res = shutdown.changed() => res.is_err() || *shutdown.borrow(),
    }
}

/// Uniform jitter in `[ms/2, ms]` so N channels reconnecting to one dead
/// gateway don't thundering-herd it.
fn jitter(ms: u64) -> u64 {
    ms / 2 + fastrand::u64(0..=ms.div_ceil(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::FakeSink;
    use mb_types::{ByteOrder, DataType, WordOrder};

    fn field(tag: u32, off: u16, len: u16) -> crate::plan::Field {
        crate::plan::Field {
            tag: TagId(tag),
            word_offset: off,
            word_len: len,
            data_type: DataType::U16,
            word_order: WordOrder::BigEndian,
            byte_order: ByteOrder::BigEndian,
            bit: None,
        }
    }

    #[test]
    fn scatter_slices_registers_by_field_offsets_into_one_batch() {
        let sink = FakeSink::default();
        let txn = Transaction {
            req: ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 6 },
            base: 100,
            fields: vec![field(0, 0, 2), field(1, 2, 1), field(2, 5, 1)],
            coalesced: true,
        };
        scatter(&sink, &txn, ModbusResponse::Registers(vec![10, 11, 12, 13, 14, 15]));

        let calls = sink.publish_calls();
        assert_eq!(calls.len(), 1, "one publish_batch per transaction");
        assert_eq!(
            calls[0],
            vec![
                (TagId(0), RawValue::Registers(vec![10, 11].into())),
                (TagId(1), RawValue::Registers(vec![12].into())),
                (TagId(2), RawValue::Registers(vec![15].into())),
            ]
        );
        assert!(sink.quality_calls().is_empty());
    }

    #[test]
    fn scatter_slices_bits_and_degrades_out_of_range_fields() {
        let sink = FakeSink::default();
        let txn = Transaction {
            req: ModbusRequest::ReadCoils { addr: 0, qty: 3 },
            base: 0,
            fields: vec![field(0, 0, 1), field(1, 2, 1), field(2, 7, 1)],
            coalesced: true,
        };
        // Server answered fewer bits than the plan expects for field 2.
        scatter(&sink, &txn, ModbusResponse::Bits(vec![true, false, true]));

        assert_eq!(
            sink.publish_calls(),
            vec![vec![
                (TagId(0), RawValue::Bits(vec![true].into())),
                (TagId(1), RawValue::Bits(vec![true].into())),
            ]]
        );
        assert_eq!(sink.quality_calls(), vec![(vec![TagId(2)], Quality::Bad)]);
    }

    #[test]
    fn scatter_rejects_wrong_total_length_entirely() {
        // #6: a reply whose total size contradicts the request qty is a stale
        // or malformed frame — nothing from it may be published Good.
        let sink = FakeSink::default();
        let txn = Transaction {
            req: ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 6 },
            base: 100,
            fields: vec![field(0, 0, 2), field(1, 2, 1)],
            coalesced: true,
        };
        // Short reply: both fields are in range of the 4 returned words,
        // but the total (4 != 6) proves the frame is not ours.
        scatter(&sink, &txn, ModbusResponse::Registers(vec![10, 11, 12, 13]));
        assert!(sink.publish_calls().is_empty(), "nothing published from a short reply");
        assert_eq!(
            sink.quality_calls(),
            vec![(vec![TagId(0), TagId(1)], Quality::Bad)]
        );

        // Long reply is equally untrusted.
        let sink = FakeSink::default();
        scatter(&sink, &txn, ModbusResponse::Registers(vec![0; 8]));
        assert!(sink.publish_calls().is_empty());
        assert_eq!(sink.quality_calls().len(), 1);

        // Short BIT reply: all Bad too.
        let sink = FakeSink::default();
        let bit_txn = Transaction {
            req: ModbusRequest::ReadCoils { addr: 0, qty: 5 },
            base: 0,
            fields: vec![field(2, 0, 1)],
            coalesced: false,
        };
        scatter(&sink, &bit_txn, ModbusResponse::Bits(vec![true, false]));
        assert!(sink.publish_calls().is_empty());
        assert_eq!(sink.quality_calls(), vec![(vec![TagId(2)], Quality::Bad)]);
    }

    #[test]
    fn scatter_checks_custom_expect_len() {
        let sink = FakeSink::default();
        let txn = Transaction {
            req: ModbusRequest::Custom { code: 65, data: vec![], expect_len: Some(4) },
            base: 0,
            fields: vec![field(3, 0, 4)],
            coalesced: false,
        };

        // Matching length publishes the raw payload.
        scatter(&sink, &txn, ModbusResponse::Raw(bytes::Bytes::from_static(&[1, 2, 3, 4])));
        assert_eq!(
            sink.publish_calls(),
            vec![vec![(TagId(3), RawValue::Raw(bytes::Bytes::from_static(&[1, 2, 3, 4])))]]
        );

        // Length mismatch: nothing published, tag degraded Bad.
        scatter(&sink, &txn, ModbusResponse::Raw(bytes::Bytes::from_static(&[1, 2])));
        assert_eq!(sink.publish_calls().len(), 1, "no second publish");
        assert_eq!(sink.quality_calls(), vec![(vec![TagId(3)], Quality::Bad)]);
    }

    #[test]
    fn timeout_is_not_retryable_on_half_duplex() {
        // #4/#12: never re-issue a timed-out request on a stream transport —
        // the late reply could alias the retry.
        assert!(!is_retryable(&ProtoError::Timeout, true));
        assert!(is_retryable(&ProtoError::Timeout, false));
        for half_duplex in [true, false] {
            // Transient exceptions keep their retry budget on any transport:
            // the slave answered, so the stream is in sync.
            assert!(is_retryable(
                &ProtoError::Exception(ExceptionCode::ServerDeviceBusy),
                half_duplex
            ));
            assert!(is_retryable(
                &ProtoError::Exception(ExceptionCode::GatewayTargetDevice),
                half_duplex
            ));
            // Deterministic exceptions and fatal errors never retry.
            assert!(!is_retryable(
                &ProtoError::Exception(ExceptionCode::IllegalDataAddress),
                half_duplex
            ));
            assert!(!is_retryable(&ProtoError::NotConnected, half_duplex));
        }
    }

    #[test]
    fn probe_of_offline_device_is_single_shot() {
        // #18: a probe is one request — one timeout of bus dead-time, not
        // (1 + max_retries) x request_timeout.
        let retry = RetryConfig {
            max_retries: 2,
            base_backoff_ms: 500,
            max_backoff_ms: 30_000,
        };
        assert_eq!(retry_budget(true, &retry), 2, "online: full budget");
        assert_eq!(retry_budget(false, &retry), 0, "offline probe: no retries");
    }

    #[test]
    fn jitter_stays_in_half_open_band() {
        for ms in [0u64, 1, 2, 500, 30_000] {
            for _ in 0..64 {
                let j = jitter(ms);
                assert!(j >= ms / 2 && j <= ms, "jitter({ms}) = {j} out of [ms/2, ms]");
            }
        }
    }
}
