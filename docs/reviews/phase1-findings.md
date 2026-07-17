# Phase-1 review findings (raw, unverified) — 33 total

## 1. [CRITICAL] [scheduling] Half-duplex dead-slave livelock: second-consecutive-timeout reconnect + on_connected() reset means the offline watchdog can never trip, and devices scheduled after the dead one are starved forever with stale Good values
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:190

run_tick counts timeouts channel-wide and forces a reconnect on the second consecutive one: `if half_duplex && *consec_timeouts >= 2 { ... return TickOutcome::Reconnect; }` (channel.rs:190-196). A single dead slave produces two consecutive timed-out transactions in virtually every batch: any device with >=2 transactions in a due group, a device split across two poll groups (after a reconnect ALL intervals are overdue, so PollWheel fires every group in one batch), or two dead devices adjacent in batch order. `continue 'devices` at channel.rs:200 only triggers once the device is already offline, so while it is still 'online' its transactions are attempted back-to-back. The reconnect then executes `for r in &mut rt { r.on_connected(); }` (channel.rs:77-79), and `on_connected` does `self.fails = 0; self.online = true; ... next_probe_at = Instant::now()` (device.rs:52-57). Consequences with defaults (offline_after=3, max_retries=2, request_timeout=1000ms): (1) `fails` reaches at most 2 before the reset, so the device NEVER crosses offline_after — no Bad sweep, no probe gating, ever; even if offline_after=2 and the sweep fires, the very same iteration returns Reconnect and on_connected flips it back online, so probe backoff never holds. (2) Each cycle burns 2 txns x (1+max_retries) x request_timeout = ~6s of bus time on the dead slave, then jitter(500ms) backoff (reset to base at channel.rs:76 on every successful connect — serial open and RTU-over-TCP terminal servers accept regardless of bus state), forever. (3) run_tick returns Reconnect from inside the 'devices loop, dropping the rest of the due batch; after reconnect the batch is rebuilt in the same priority/device order, so every device ordered after the dead one is never polled again — and since the connect itself succeeds, their tags are never swept Bad: they show stale values with Good quality indefinitely. The TCP-only e2e tests never exercise this path (tests/e2e.rs uses transport type tcp, half_duplex=false). At the stated scale (>100 devices, multi-transaction register maps) one permanently dead RTU slave takes down the whole channel.

**Suggested fix:** Do not let on_connected() erase the offline watchdog: preserve `fails`/`online`/`next_probe_at` across reconnects (only reset probe state on an actual successful request to that device), and/or exempt the consec-timeout stream-drain rule when the timeout came from a device already counting toward offline (no bytes were ever received from it, so there is nothing to desync). Also make run_tick resume the remainder of the due batch after a reconnect instead of dropping it (e.g. carry the unfinished batch across 'reconnect).

---

## 2. [CRITICAL] [quality-efficiency] Offline watchdog is unreachable on half-duplex transports: consec-timeout reconnect wipes the failure counter
**File:** crates/mb-poller/src/channel.rs:190

Three pieces interact fatally on RTU / RTU-over-TCP. (1) channel.rs:190-196: `if half_duplex && *consec_timeouts >= 2 { ... return TickOutcome::Reconnect; }` — two consecutive transaction timeouts force a channel reconnect. (2) channel.rs:77-79 after every successful (re)connect: `for r in &mut rt { r.on_connected(); }`. (3) device.rs:52-57 `on_connected` does `self.fails = 0; self.online = true; self.next_probe_at = Instant::now();`. Serial-port open and terminal-server TCP connect virtually always succeed even when the slave is dead, so a dead slave that produces two back-to-back timeouts (any device with >=2 transactions per tick — e.g. coils + holding, or two non-coalescable spans — or two dead devices adjacent in the due batch) drives the cycle: timeout, timeout -> reconnect -> fails reset to 0 -> repeat forever. With the default `offline_after = 3` (schema d_3) `fails` can only ever reach 2, so: the device never flips offline, `sink.set_device_quality(all_tags, Bad)` never fires (tags stay Uncertain forever, contradicting the design §7 table 'Timeout -> Bad (at threshold)'), probe-backoff gating (`is_offline_and_not_due_to_probe`) never engages, and the channel reopens the serial port every ~2 ticks with a 250-500 ms backoff pause each time — exactly the 'dead slave starves healthy slaves' scenario §4 says the watchdog exists to prevent. Each probe also still costs `(1 + max_retries) * request_timeout` inside poll_with_retry (channel.rs:228-258), i.e. 3 s of bus time per attempt at defaults. On TCP the rule is skipped and the watchdog works, which hides the bug from all existing (TCP-only) tests.

**Suggested fix:** Preserve DeviceRuntime failure/offline state across timeout-triggered reconnects. E.g. make on_connected() keep `fails` and the offline/probe state (a fresh connection says nothing about a slave that just timed out twice), or only clean-slate runtimes when the previous disconnect was NOT caused by the consec-timeout rule (pass the reconnect reason into the loop). Alternatively satisfy the §7 stream-drain rule without a full transport teardown for the counter: reconnect the socket but re-apply saved DeviceRuntime state. Add a half-duplex integration test with a non-answering unit id to lock the behavior in.

---

## 3. [MAJOR] [modbus] Failed probe of an offline device upgrades tag quality from Bad to Uncertain
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:181

In run_tick's timeout arm:

    Err(_) => {
        *consec_timeouts += 1;
        if rt[dev_idx].on_comm_failure() {
            sink.set_device_quality(&dev.all_tags, Quality::Bad);
        } else {
            sink.degrade_uncertain(&txn.tags()); // stale-but-maybe-recovering
        }

`DeviceRuntime::on_comm_failure` (device.rs:72-91) returns `true` only at the moment the device *crosses* the offline threshold; when the device is already offline it takes the `else` branch (escalate probe backoff) and returns `false`. So every failed probe of a dead device lands in `sink.degrade_uncertain(&txn.tags())`, flipping the probed transaction's tags from Bad (set by the threshold-crossing sweep) back to Uncertain — quality *improves* while the device stays dead, and it stays Uncertain until the next successful read or reconnect sweep. This contradicts the design §7 table: Timeout -> "Uncertain (below threshold) -> Bad (at threshold)"; once at/over the threshold the tags must remain Bad. Concrete scenario: offline_after=3, device dies, tags swept Bad on the 3rd timeout; the first probe (base_backoff later) times out and the first transaction's tags are now shown Uncertain to OPC UA/MQTT for the remainder of the outage.

**Suggested fix:** Only degrade to Uncertain while the device is still online: `} else if rt[dev_idx].is_online() { sink.degrade_uncertain(&txn.tags()); }` (a failed probe of an already-offline device should leave quality Bad untouched).

---

## 4. [MAJOR] [scheduling] Retry-after-timeout on a half-duplex stream can lock onto one-frame-shifted responses: late replies get scattered into the wrong transaction's tags with Good quality
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:245

poll_with_retry treats `ProtoError::Timeout` as retryable and immediately re-issues the identical request on the same connection (channel.rs:245-257, only `inter_request_delay` — default 20ms — in between). tokio-modbus 0.17 clears only the codec's already-buffered bytes before each send (`framed.read_buffer_mut().clear();`, tokio-modbus src/service/rtu.rs:65) and validates a response only by slave id and function code (src/service/rtu.rs:79-95); there is no request/response correlation and no quantity check for reads. Scenario: slave answers at 1.2s with request_timeout=1000ms. Request A times out at t=1.0; retry A' is sent at t=1.02; A's late reply arrives at t=1.2 and — being an answer to a byte-identical request — is accepted as A''s response ('success', `*consec_timeouts = 0` at channel.rs:161). A''s real reply then arrives while transaction B is in flight; same slave and (on a typical homogeneous FC03 map) same function code, so it is accepted as B's response and `scatter` (channel.rs:301-353) publishes A's registers into B's tags with Good quality. The stream stays shifted by one frame across subsequent transactions until a function-code or slice-length mismatch finally surfaces. The design's own mitigation — the §7 second-consecutive-timeout drain rule (channel.rs:190) — never fires, because after the first timeout every exchange 'succeeds'. A device that is systematically slightly slower than request_timeout (classic misconfiguration) publishes misattributed data continuously.

**Suggested fix:** On a half-duplex transport, do not silently retry a Timeout on the same connection. Either (a) treat the first Timeout on a stream transport as requiring a drain: sleep >= request_timeout (or explicitly read-and-discard until silence) before the retry so any late reply is consumed/expired first, or (b) make Timeout non-retryable on half-duplex and let the txn fail (the existing consec_timeouts rule then forces the clean reconnect the design intends).

---

## 5. [MAJOR] [modbus] Write path bypasses the second-consecutive-timeout stream-drain rule (and the timeout metric / failure counter)
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:266

The §7 stream-drain rule ("a dropped in-flight exchange can desync a byte stream; a second consecutive timeout forces a clean reconnect") is implemented only for poll transactions via `consec_timeouts` in run_tick (channel.rs:181,190). `service_write` performs the exact same kind of exchange on the same half-duplex stream (`tx.request(dev.unit, &cmd.req, ...)`, channel.rs:282) but: (1) never increments or checks `consec_timeouts` — it only escalates on `e.is_fatal()`, and `ProtoError::Timeout` is non-fatal; (2) never bumps `metrics.timeouts` (only reqs_err/writes_err); (3) never calls `on_comm_failure` for the device. Consequences on RTU/RTU-over-TCP: a write timeout leaves a potentially in-flight reply exactly like a read timeout, yet two consecutive write timeouts (or write-timeout followed by poll-timeout) never trigger the forced reconnect. The dangerous concrete case: write W1 (FC06) times out; a second queued write W2 (FC06, different register) is sent next; W1's late echo has the same slave id and function code, so tokio-modbus accepts it as W2's reply — and the echo address/value check in `write_single_register` is `debug_assert_eq!` only (vendored client/mod.rs:293-296), so in a release build the operator's oneshot receives Ok for a write whose real acknowledgement was never seen, while W2's true echo stays in flight and poisons the next exchange.

**Suggested fix:** Thread `&mut consec_timeouts` (and `half_duplex`) into `service_write`; on `Err(ProtoError::Timeout)` increment it, bump `metrics.timeouts`, and return "reconnect" when `half_duplex && consec_timeouts >= 2`, mirroring run_tick. Reset the counter on Ok/Exception like the poll path does.

---

## 6. [MAJOR] [modbus] scatter() trusts a response of the wrong total length: in-range fields from a short/stale reply are published Good
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:308

scatter only bounds-checks each field individually:

    ModbusResponse::Registers(regs) => {
        for f in &txn.fields {
            let start = f.word_offset as usize;
            match regs.get(start..start + f.word_len as usize) {
                Some(words) => updates.push((f.tag, RawValue::Registers(...))),
                None => bad.push(f.tag),
            }

It never compares `regs.len()` (or `bits.len()`) against the requested `qty` in `txn.req`. Two failure modes: (a) a response *shorter* than qty is by Modbus contract malformed, yet fields that happen to fit are still published Good (only the tail fields degrade Bad); (b) a response *longer* than qty passes entirely. This matters because tokio-modbus validates register-response length only with `debug_assert_eq!(words.len(), cnt.into())` (vendored client/mod.rs:182, 201) — in release builds any length flows through. The realistic trigger is the timeout-desync race the design itself acknowledges (§7 stream-drain rule): request A times out, `poll_with_retry` re-sends the same PDU and *succeeds* by consuming A's late reply (resetting `consec_timeouts` to 0), leaving A'-retry's true reply in flight; the next transaction to the same slave with the same FC but different addr/qty then decodes that stale reply cleanly (RTU framing is self-describing via the byte-count field, and header/function-code checks pass), and scatter publishes register values read from the *previous transaction's addresses* as Good quality. A `len == qty` check in scatter is the cheap last line of defense that turns this into a tag-scoped Bad instead of silent wrong data.

**Suggested fix:** In scatter, extract the requested qty from `txn.req` and reject mismatched totals before slicing: for Registers require `regs.len() == qty as usize`, for Bits require `bits.len() >= qty as usize` (tokio-modbus truncates coils to cnt, so > is impossible from the happy path but < indicates a short reply); on mismatch mark all `txn.tags()` Bad, mirroring the existing Custom expect_len handling.

---

## 7. [MAJOR] [scheduling] Reconnect backoff resets on successful connect, not on first successful request: connect-OK/request-always-fatal loops spin at base backoff forever and tags are never marked Bad
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:76

Immediately after `Transport::connect` succeeds, `backoff_ms = plan.retry.base_backoff_ms;` (channel.rs:76) — before any request has succeeded. For RTU serial, `rtu_serial` is just `SerialStream::open` (mb-proto/src/connect.rs:48-50), which succeeds whenever the device node exists, regardless of bus health; RTU-over-TCP terminal servers likewise accept TCP even when the serial side is dead. If the first request then fails fatally (Io on write, instant RST, garbage->ProtocolError), the cycle is: connect (instant) -> fatal request (instant) -> backoff_or_stop sleeps jitter(base)=250-500ms -> connect resets backoff to base -> repeat. The exponential escalation to max_backoff_ms=30s (channel.rs:363-366) is unreachable in exactly the scenario it exists for, giving ~2-4 reconnect cycles per second indefinitely. Worse for data quality: the connect-failure path sweeps all tags Bad (channel.rs:66-68), but this connect-success/request-fatal path only sweeps a device Bad when `on_comm_failure()` crosses offline_after (channel.rs:169-171) — and `on_connected()` at channel.rs:77-79 resets `fails` to 0 every cycle, so with offline_after=3 the counter alternates 0->1 and the threshold is never crossed. All tags keep their last values with Good quality while the channel is completely non-functional.

**Suggested fix:** Reset `backoff_ms` to base only after the first successful request on the new connection (e.g. set a `healthy = false` flag at connect, flip it in the Ok arm of run_tick/service_write, and only then reset backoff). Keep doubling it while every connection dies without a single successful exchange. Optionally treat 'connected but zero successful requests' the same as connect failure for the whole-channel Bad sweep after N cycles.

---

## 8. [MAJOR] [scheduling] Writes are only serviced between whole scheduler batches, not between transactions: operator write latency is unbounded by the size of a tick batch (seconds to tens of seconds at target scale)
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:95

The comment (channel.rs:95-97) and design §8 claim writes are serviced 'only between whole transactions ... so operator control actions aren't delayed behind slow polls'. But the select! granularity is one entire `run_tick` batch: `due = wheel.next_due() => { let outcome = run_tick(due, ...).await; }` (channel.rs:112-116), and run_tick sequentially executes EVERY due (device, group) pair and every transaction within them, including full retry budgets, before control returns to select! (channel.rs:142-210). After a reconnect all intervals are overdue, so the first batch contains every group of every device. Worst cases with defaults (request_timeout=1000ms, max_retries=2): one flaky-but-online device with 3 due transactions blocks a pending write for 3 x 3s = 9s; a batch of 100 healthy RTU devices at 9600 baud with inter_request_delay=20ms blocks it for multiple seconds on every tick. The write path itself is fine (service_write replies via oneshot, honors inter_request_delay at channel.rs:107-109), but the promised priority only holds between batches, not between transactions.

**Suggested fix:** Drain pending writes between transactions inside run_tick: after each transaction (where the inter_request_delay sleep already sits, channel.rs:204-207), do a non-blocking `writes.try_recv()` loop and call service_write for anything queued. This preserves the no-mid-frame invariant (still a transaction boundary) while bounding write latency to one transaction (~request_timeout x (1+max_retries)) as the design text promises.

---

## 9. [MAJOR] [modbus] Per-device request_timeout_ms and retry overrides are resolved but silently ignored by the poller
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\plan.rs:119

The schema documents `DeviceConfig.request_timeout_ms` / `DeviceConfig.retry` as "Overrides channel" and the design (§2) requires "per-device timeouts/retries ... fully resolved (device override -> channel -> gateway default)". `resolve.rs:160-161` duly computes `request_timeout_ms: dev.request_timeout_ms.unwrap_or(ch.request_timeout_ms)` and `retry: dev.retry.unwrap_or(ch.retry)` into `ResolvedDevice` — but the plan compiler drops both: `ChannelPlan` carries only the channel-level `request_timeout: Duration::from_millis(ch.request_timeout_ms)` (plan.rs:119) and `retry: ch.retry` (plan.rs:122), and `DevicePlan` (plan.rs:37-47) has no timeout/retry fields. At runtime every request uses `plan.request_timeout` (channel.rs:228, 282), every `DeviceRuntime` gets `plan.retry` (channel.rs:48), and `poll_with_retry` uses `plan.retry.max_retries` (channel.rs:252). Net effect: a slow slave configured with e.g. `request_timeout_ms: 2000` on a 500 ms channel will chronically time out, be marked offline, and burn retries — with the user's documented knob doing nothing. `GatewaySettings.default_retry` is likewise never consulted anywhere.

**Suggested fix:** Carry `request_timeout: Duration` and `retry: RetryConfig` in `DevicePlan` (from `ResolvedDevice`), use `dev.request_timeout` in `poll_with_retry`/`service_write` and `dev.retry` when constructing each `DeviceRuntime` and computing max_retries; fall back through channel to `gateway.default_retry` in resolve.rs.

---

## 10. [MAJOR] [config] gateway.default_retry is parsed but never consulted — documented override precedence 'device → channel → gateway default' is only two-level
**File:** crates/config/src/resolve.rs:161

docs/phase1-design.md:288 specifies: "per-device timeouts/retries/`max_gap`/`offline_after` fully resolved (device override → channel → gateway default)". The schema exposes the gateway level (`crates/config/src/schema/v1.rs:20`: `pub default_retry: RetryConfig` inside `GatewaySettings`), so a user can set it, and it round-trips through load. But resolve.rs only implements two levels: `retry: dev.retry.unwrap_or(ch.retry)` (resolve.rs:161) and `retry: ch.retry` (resolve.rs:180). A grep for `default_retry` across all crates shows its ONLY occurrence is the schema declaration — no reader exists. Because `ChannelConfig.retry` is non-optional with `#[serde(default)]` (schema/v1.rs:43-44), a channel that omits "retry" in JSON silently gets the hard-coded `RetryConfig::default()` (2 retries / 500 ms / 30 s) and `gateway.default_retry` can never take effect. Concrete failure: an operator sets `"gateway": { "default_retry": { "max_retries": 0, "max_backoff_ms": 5000 } }` expecting fleet-wide behavior; every channel silently runs with 2 retries and 30 s max backoff instead. Config is accepted with no warning, so the misconfiguration is invisible until observed at runtime.

**Suggested fix:** Either implement the third level (make `ChannelConfig.retry` an `Option<RetryConfig>` and resolve as `dev.retry.or(ch.retry).unwrap_or(cfg.gateway.default_retry)` — same for any other knob intended to have a gateway default), or delete `default_retry` from `GatewaySettings` and the design doc so unknown-intent config cannot be silently ignored. If keeping the field is deferred, at minimum emit a validation warning when `gateway.default_retry` differs from `RetryConfig::default()`.

---

## 11. [MAJOR] [concurrency] Shutdown is only observed at the select! boundary: a running tick can delay shutdown()/task join by many minutes at the 100-device scale target
**File:** crates/mb-poller/src/channel.rs:112-124

In the poll arm `due = wheel.next_due() => { let outcome = run_tick(...).await; ... }` the whole batch runs to completion before select! re-polls the shutdown arm. Neither run_tick (channel.rs:132-212) nor poll_with_retry (channel.rs:218-262) checks the shutdown watch. On a full-duplex TCP channel the consec_timeouts>=2 escape does not apply (`half_duplex` is false), so with a reachable-but-dead gateway (TCP accepts, slaves silent) every device in the due batch burns `offline_after × (max_retries+1) × request_timeout` = 3×3×1s = 9 s with defaults before `continue 'devices`; at the stated scale target (>100 devices on a channel) one tick can take ~15 minutes, during which `PollerHandle::shutdown()` (lib.rs:69-74: `for t in self.tasks { let _ = t.await; }`) blocks. The e2e test's 5 s shutdown bound (tests/e2e.rs:138-140) only passes because it has one healthy device. `Transport::connect` at channel.rs:59 is likewise not raced against shutdown (bounded by connect_timeout, default 5 s — minor contributor).

**Suggested fix:** Check shutdown inside run_tick's device loop (cheap: `if *shutdown.borrow() { return TickOutcome::Continue }` between transactions, passing the receiver down) and/or wrap each `tx.request` in `tokio::select!` with `shutdown.changed()`. Between transactions is enough — it keeps the half-duplex invariant (never cancel a request mid-flight on RTU; on shutdown mid-request, waiting out one request_timeout is acceptable).

---

## 12. [MAJOR] [concurrency] Stale-response aliasing on half-duplex defeats the second-consecutive-timeout rule: wrong data published as Good and the counter self-resets
**File:** crates/mb-poller/src/channel.rs:158-165

The drain rule fires only on two *consecutive transaction-level* timeouts (channel.rs:190-196). But a single timeout followed by a stale-aliased 'success' resets the counter: `Ok(resp) => { *consec_timeouts = 0; ... scatter(sink, txn, resp); }`. Scenario on RTU/RTU-over-TCP with a slave that answers just above `request_timeout` (default 1000 ms, `inter_request_delay` default 0 — schema/v1.rs:34-36): txn A (FC03, qty 10) times out after exhausting retries; run_tick moves to txn B (FC03, qty ≤10, different address, same unit). A's late reply arrives after B was sent (tokio-modbus clears only pre-send buffered bytes, service/rtu.rs:65), so B consumes A's frame: slave id matches, FC matches, and the register-count check is `debug_assert_eq!(words.len(), cnt.into())` (tokio-modbus client/mod.rs:201) — compiled out in release. scatter() only degrades fields that fall *outside* a short response (channel.rs:311-314); if the stale reply is >= B's qty, B's tags get A's register values with Quality::Good, `consec_timeouts` resets to 0, and B's own late reply then aliases txn C — a persistent off-by-one cascade the 2-consecutive rule never sees because every read 'succeeds'. Aggravating factor: poll_with_retry resends up to `max_retries` times after Timeout on the same connection (channel.rs:245-258) with no drain, so up to max_retries+1 abandoned replies can be in flight while the outer counter records a single timeout.

**Suggested fix:** Do not trust the first exchange after a timeout on a stream transport. Options: (a) after any transaction-level Timeout on half-duplex, drain/discard input for one `request_timeout` (or reconnect immediately, dropping the >=2 heuristic); (b) validate response payload length against the request qty in mb-proto (release-mode check, not debug_assert) and treat mismatch as ProtoError::Protocol -> fatal; (b) is cheap and catches most aliasing since consecutive transactions rarely share exact qty.

---

## 13. [MAJOR] [concurrency] Write path bypasses the half-duplex stream-drain rule: a timed-out write never counts toward consec_timeouts and never forces reconnect
**File:** crates/mb-poller/src/channel.rs:98-110

The §7 drain rule ('second consecutive timeout on a stream transport is fatal') is implemented only inside run_tick via the local `consec_timeouts` counter (channel.rs:81, 181, 190). The write arm does not participate: `Some(cmd) = writes.recv() => { let fatal = service_write(cmd, &plan, &mut tx, &metrics).await; if fatal { ...reconnect... } }` and service_write returns `let fatal = matches!(&res, Err(e) if e.is_fatal());` (channel.rs:293) — `ProtoError::Timeout` is not fatal (error.rs:28-30), so a timed-out write neither increments `consec_timeouts` nor triggers reconnect. On RTU / RTU-over-TCP the timed-out `tokio::time::timeout` in `Transport::request` (transport.rs:132-135) drops the exchange mid-flight, leaving the slave's late reply in the stream. tokio-modbus only clears bytes buffered *before* the next send (`framed.read_buffer_mut().clear()`, service/rtu.rs:65), so a reply arriving after the next send is consumed by the next request. Concrete failure: OPC UA queues two WriteSingleRegister commands to the same unit (biased select drains them back-to-back). Write A times out; write B is sent; A's late FC06 echo arrives and is consumed as B's response — slave id matches, function code matches, and the echoed addr/value are only checked by `debug_assert_eq!` (tokio-modbus client/mod.rs:291-293), i.e. not at all in release builds. B's submitter receives Ok(WriteAck) that actually acknowledges A, and the stream stays permanently off-by-one until a later FC mismatch happens to force a reconnect. If a poll follows instead, it eats the FC06 frame, gets FunctionCodeMismatch -> fatal reconnect — an avoidable full-channel reconnect caused by an uncounted, undrained write timeout.

**Suggested fix:** Treat a write timeout on a half-duplex transport the same as a poll timeout: pass `&mut consec_timeouts` into service_write (increment on Timeout, reset on Ok), and apply the same `half_duplex && consec_timeouts >= 2 -> reconnect` rule there. Simpler and safer: on any Timeout of a write on a half-duplex transport, force `continue 'reconnect` immediately (a write burst is rare; correctness of acks matters more than one reconnect).

---

## 14. [MAJOR] [quality-efficiency] WriteCommand.device_idx indexes the enabled-filtered plan; no safe public mapping — writes can silently target the wrong slave
**File:** crates/mb-poller/src/command.rs:12

`WriteCommand { pub device_idx: usize, ... }` is documented as 'Resolved index into the channel's `ChannelPlan::devices` vec', but `ChannelPlan::devices` is built in plan.rs:109-114 with `.filter(|d| d.enabled)` — so the index space is the *enabled-only* subsequence, which differs from `ResolvedConfig.channels[i].devices` whenever any device is disabled. There is no public API to compute this index: `ChannelPlan` is consumed by `Poller::spawn` and `PollerHandle` only exposes `writer(ChannelId)` (lib.rs:59-61). A phase-3 OPC UA/MQTT layer holding a `DeviceId` from `ResolvedConfig` must re-implement plan.rs's filtering to derive `device_idx`; if it naively uses the config-order index and one earlier device is disabled, `service_write` (channel.rs:272-282) resolves a *different* device and sends the write to the wrong `unit` — an out-of-range index is rejected (channel.rs:272-280) but an in-range wrong index is not detectable. On a control write path that is a wrong-actuator hazard, not just an API wart.

**Suggested fix:** Key writes by identity, not position: put `DeviceId` (or unit id) in `WriteCommand` and let the channel task resolve it against its plan (a small DeviceId->idx map built once in run_channel), replying with an explicit UnknownDevice error on miss. Alternatively expose `PollerHandle::write_target(DeviceId) -> Option<(ChannelId, usize)>` built from the plans at spawn time, and document device_idx as opaque.

---

## 15. [MAJOR] [quality-efficiency] Per-device request_timeout_ms and retry overrides are validated and resolved but silently ignored by the poller
**File:** crates/mb-poller/src/plan.rs:119

The schema documents `DeviceConfig.request_timeout_ms` as 'Overrides channel' (schema/v1.rs:97-101) and resolve.rs:160-161 dutifully computes `request_timeout_ms: dev.request_timeout_ms.unwrap_or(ch.request_timeout_ms)` and `retry: dev.retry.unwrap_or(ch.retry)` per device. The design (§2, line 288) promises 'per-device timeouts/retries ... fully resolved'. But the plan compile step drops both: plan.rs:119 `request_timeout: Duration::from_millis(ch.request_timeout_ms)` and plan.rs:122 `retry: ch.retry` keep only channel-level values, `DevicePlan` (plan.rs:36-47) has no timeout/retry fields, and channel.rs:48 builds every runtime from `DeviceRuntime::new(d.offline_after, plan.retry)` while poll_with_retry uses `plan.request_timeout` / `plan.retry.max_retries` (channel.rs:228, 252). Net effect: a user who sets a 750 ms timeout on one slow meter (as the shipped sample-config does — config/tests/sample.rs:57 even asserts the resolved value) gets the channel's timeout at runtime with no warning. `dev.offline_after` and `dev.max_gap` ARE honored, making the two ignored knobs easy to trust by analogy.

**Suggested fix:** Carry the resolved per-device values into DevicePlan (request_timeout: Duration, retry: RetryConfig), use `plan.devices[dev_idx].request_timeout`/`retry` in poll_with_retry and service_write, and build DeviceRuntime from the device's own retry. If deferring to a later phase instead, reject or warn on the overrides in validate.rs so they are not a silent no-op.

---

## 16. [MAJOR] [quality-efficiency] Test blind spots: no test exercises timeouts, live de-coalescing, or the Custom wire path
**File:** crates/mb-poller/tests/e2e.rs:1

Top 3 concrete gaps in phase-1 behavior with NO test coverage. (1) Nothing anywhere produces a request Timeout: the sim always answers, both e2e tests are TCP-only and only kill the whole server. Consequently `degrade_uncertain` wiring from run_tick (channel.rs:185), the Uncertain->Bad threshold progression, poll_with_retry's retry-on-timeout (channel.rs:245-256), probe-backoff gating in a live loop, and the half-duplex consec-timeout>=2 reconnect rule are all untested — FakeSink::degrade_calls is literally `#[allow(dead_code)]` (lib.rs:140-141) because no unit test asserts it either. The critical watchdog bug above lives exactly in this hole. (2) Adaptive de-coalescing is unit-tested only at the DeviceRuntime level (device.rs tests); no test drives run_channel to receive IllegalDataAddress on a coalesced read and verifies subsequent ticks poll the remembered split via `effective_txns` (channel.rs:152-156) and recover the good fields — the sim already returns IllegalDataAddress for unmapped addresses, so this is cheap to add. (3) FunctionCode::Custom has zero end-to-end coverage: no mb-proto integration test sends a Custom request (grep 'Custom' in crates/mb-proto/tests = 0 matches) and the poller sims only handle FC03/FC06, even though the design (§10 item 4) calls Custom framing 'the highest residual risk'; scatter's expect_len check is unit-tested against hand-built responses only.

**Suggested fix:** Add: (a) a half-duplex (rtu_over_tcp against the sim) e2e with one non-answering unit asserting Uncertain-then-Bad, probe gating, and no reconnect churn; (b) an e2e where one bridged register is unmapped, asserting the split happens once and the remaining tags return to Good; (c) an mb-proto integration test for Request::Custom over TCP and rtu-over-tcp, including a short-reply expect_len mismatch.

---

## 17. [MAJOR] [config] custom_response_len is enforced by validation but never used to delimit the RTU response — Custom reads on stream transports rely on a buffer-timing race in tokio-modbus
**File:** crates/mb-proto/src/transport.rs:121

Validation rule 6 (validate.rs:164-180) hard-requires `custom_response_len` for Custom reads on Rtu/RtuOverTcp, with the error text "the byte stream cannot self-delimit vendor frames" (lib.rs:66-70), matching design §3.2/§6. But the value never reaches the framing layer. transport.rs:121-129 handles Custom via `flatten(ctx.call(req.to_tokio_request()).await)`, and request.rs:50 drops `expect_len` on conversion (`Self::Custom { code, data, .. } => Request::Custom(...)`); it is only checked *post-hoc* in `scatter` (channel.rs:326-343) after a response was already framed. tokio-modbus 0.17's RTU ClientCodec frames unknown function codes as `_ => if adu_buf.len() >= 3 { adu_buf.len() - 3 }` (codec/rtu.rs:218-226), i.e. "whatever bytes are currently buffered", then `FrameDecoder::decode` immediately CRC-checks that truncated slice (codec/rtu.rs:47-70) and on mismatch `recover_on_error` drops a byte (codec/rtu.rs:86-104). On a real serial port bytes trickle in across multiple reads, so for any Custom reply longer than a few bytes the decoder fires on a partial buffer, fails CRC, and starts byte-dropping — the frame is consumed as garbage and leftover bytes desync the next transaction (→ Protocol error → fatal reconnect, per channel.rs:167-172). So a config that passes validation (Custom + custom_response_len on RTU) still yields tags that are permanently Bad plus reconnect churn on the shared bus, punishing the other devices on that channel. The validated invariant exists precisely to prevent this and is not wired through.

**Suggested fix:** Use `expect_len` for actual delimiting on stream transports: bypass `Context::call` for Custom on Rtu/RtuOverTcp and read exactly `1 (addr) + 1 (fc) + expect_len + 2 (crc)` bytes (with the exception-frame special case fc|0x80 → 2-byte PDU) before handing the PDU to the decoder, or upstream a length hint into tokio-modbus. Until then, document that Custom on serial RTU is unreliable and consider a validation warning rather than silently accepting it.

---

## 18. [MINOR] [scheduling] Offline-device probe burns the full retry budget — (1+max_retries) x request_timeout of bus dead-time per probe instead of the single timeout the design intends
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:159

When an offline device's probe is due (`is_offline_and_not_due_to_probe()` false, channel.rs:144), its first transaction goes through the normal `poll_with_retry(...)` (channel.rs:159), which retries Timeout up to `plan.retry.max_retries` times (channel.rs:245-255). With defaults (max_retries=2, request_timeout=1000ms) each probe of a dead slave blocks the bus for ~3s, not the 'full timeout' (singular) that device.rs's module doc promises ('a dead slave cannot burn a full timeout every fast-group tick'). Early in the probe-backoff escalation (base_backoff_ms=500 -> 1s -> 2s...) the probe interval is SHORTER than the probe's own burn time, so shortly after a device goes offline the bus spends the majority of its time timing out on it; even at the max_backoff_ms=30s cap, each dead device permanently consumes ~10% of bus time (3s of every 30s). Retrying a probe is also pointless by construction: the device already failed `offline_after` consecutive requests, and `next_probe_at` is only computed after the retries complete (device.rs:83-88), so the extra attempts just push healthy devices' polls out. (On half-duplex this is currently masked by the reconnect livelock finding; it applies as written on TCP, where devices behind a gateway do reach the offline state.)

**Suggested fix:** Skip retries for probes: pass an attempt budget of 0 to poll_with_retry when `!rt[dev_idx].is_online()` (one request, one timeout per probe), and/or ensure the probe interval is always >= the worst-case probe duration ((1+max_retries) x request_timeout) when computing next_probe_at.

---

## 19. [MINOR] [modbus] Exception replies never clear or re-arm the offline/probe state: probe gating is defeated and the device stays flagged offline
**File:** D:\123321\rust opc_ua 20 test\crates\mb-poller\src\channel.rs:174

In run_tick:

    Err(ProtoError::Exception(exc)) => {
        *consec_timeouts = 0;
        rt[dev_idx].handle_exception(group_idx, txn_idx, txn, exc, sink);
    }

Neither this arm nor `handle_exception` (device.rs:122-146) touches `online`, `fails`, or `next_probe_at`. For a device that was flipped offline by timeouts and then starts answering with exceptions (common with Modbus TCP->RTU gateways: the gateway answers 0x0B GatewayTargetDevice quickly on behalf of a dead slave, or a recovered slave rejects the coalesced read with IllegalDataAddress): (1) the device is provably reachable, but `on_success` is never called, so `online` stays false and "device back online" never fires; (2) `next_probe_at` is only advanced inside `on_comm_failure` (device.rs:78, 88), so after the first exception-answered probe it remains in the past — `is_offline_and_not_due_to_probe()` (device.rs:100-102) is permanently false and the "offline" device is polled at full rate every tick with ALL of its transactions, each `GatewayTargetDevice` reply additionally retried up to `max_retries` times by poll_with_retry (channel.rs:245-251). The exponential probe backoff that exists precisely to keep a dead slave from consuming the bus is completely bypassed, and on a shared RS-485 bus those retried exchanges (each a full round-trip) steal airtime from healthy slaves every fast-group tick.

**Suggested fix:** Treat any exception response as proof of life for the link: in the Exception arm call `rt[dev_idx].on_success()` (the slave answered; per the design table exceptions keep the device online) before `handle_exception` — or, if GatewayTargetDevice should not count as proof of life for a gatewayed slave, at minimum re-arm `next_probe_at` (escalating backoff) when the device is offline and the exception is GatewayTargetDevice/GatewayPathUnavailable.

---

## 20. [MINOR] [quality-efficiency] gateway.default_retry is parsed but never consulted anywhere
**File:** crates/config/src/schema/v1.rs:20

`GatewaySettings { pub default_retry: RetryConfig }` is deserialized, and the design (line 288) specifies the fallback chain 'device override -> channel -> gateway default'. But grep shows the only occurrence of `default_retry` in all crates is its declaration; `ChannelConfig.retry` defaults via `#[serde(default)]` to `RetryConfig::default()` (v1.rs:43-44), and resolve.rs:161 only chains device -> channel. A user setting `gateway.default_retry` gets the hardcoded serde default instead, silently.

**Suggested fix:** Either wire it: in migrate/resolve, use `gateway.default_retry` as the fallback when the channel omits `retry` (requires `retry: Option<RetryConfig>` in ChannelConfig to distinguish 'omitted' from 'explicit default'), or delete the field until phase 2.

---

## 21. [MINOR] [config] Custom function code value is completely unvalidated — code 0 or code >= 0x80 passes validation but can never succeed on the wire
**File:** crates/config/src/validate.rs:164

`validate_entry` for `FunctionCode::Custom { .. }` (validate.rs:164-181) only checks `custom_response_len`; the `code` itself is unconstrained (schema: `Custom { code: u8 }`, function.rs:17). Modbus function codes are 1..=127; bit 0x80 marks exception responses. A config with `"function": { "custom": { "code": 131 } }` (or 0) validates cleanly, but at runtime the echoed response function code 0x83 is framed as a 2-byte exception PDU by tokio-modbus (codec/rtu.rs:186: `0x81..=0xAB => 2`) and decoded as `ExceptionResponse`, so the poller sees `ProtoError::Exception` on every cycle forever — the tag is permanently Bad and, being classified retryable-or-not per exception kind, burns bus time each period. Likewise `code` colliding with a standard code (e.g. 3) sends a malformed FC03 request most slaves reject. All of this is statically knowable at load time.

**Suggested fix:** In the `FunctionCode::Custom` arm of `validate_entry`, reject `code == 0 || code >= 0x80` with a new ConfigError variant, and optionally warn when `code` collides with the standard codes handled elsewhere (1-6, 15, 16).

---

## 22. [MINOR] [config] poll_group period_ms == 0 passes validation; the poller silently clamps it to 1 ms
**File:** crates/config/src/validate.rs:20

No rule checks `PollGroupConfig::period_ms > 0` (`validate()` never inspects `period_ms` except as a divisor guarded by `p.max(&1)` at validate.rs:363). A `{ "id": "fast", "period_ms": 0 }` group (an easy typo) loads successfully; `PollWheel::new` then clamps it (schedule.rs:48: `interval(p.max(Duration::from_millis(1)))` — the comment admits "Interval panics on zero period; clamp defensively"). The result is a silent 1000-polls-per-second schedule per device in that group — on a 9600-baud RTU bus that is permanently overdue and starves other groups, and on TCP it hammers the slave. The bus-load warning partially covers serial RTU (period treated as 1 ms) but nothing covers Tcp/RtuOverTcp, and even on RTU the user only gets a utilization warning, not the actual cause.

**Suggested fix:** Add a validation rule rejecting `period_ms == 0` (e.g. `ConfigError::BadPollPeriod { group }`), keeping the PollWheel clamp as defense in depth.

---

## 23. [MINOR] [config] Bus-load estimate omits the design's request_timeout_margin / slave-turnaround term, so it under-warns; it also runs on disabled channels
**File:** crates/config/src/validate.rs:304

Design §7 rule 10 (docs/phase1-design.md:864) defines per-transaction airtime as `(frame_bytes × 11 bits/char / baud + inter_request_delay + request_timeout_margin)`. The implementation (validate.rs:304-307) computes only `(chars * 11) as f64 * 1000.0 / baud as f64 + inter_request_delay_ms as f64` — the margin term is dropped entirely. Since `inter_request_delay_ms` defaults to 0 (schema/v1.rs:35-36), the estimate models a bus where slaves answer instantly; real slave turnaround (commonly 1-50 ms/txn and dominant at high baud) is unmodeled, so a schedule that is physically infeasible (e.g. 100 devices × 1 txn at 115200 baud in a 200 ms period ≈ 143 ms pure airtime + ~500 ms turnaround) is estimated at ~72% utilization and passes silently — exactly the "permanently overdue" state the rule exists to catch. (The 11 bits/char constant itself is fine: it equals the worst case 1 start + 8 data + parity/stop + stop, so with the default 8N1 (10 bits) it only overestimates ~10%, erring safe; the frame-byte and bit-payload math — 8-byte request, 5+payload response, `div_ceil(8)` for coils, `qty*2` for registers — is correct.) Separately, the estimate loop (validate.rs:57-85) never checks `ch.enabled`, while it does filter `d.enabled` devices (validate.rs:321), so a fully disabled RTU channel still emits a BusOverload warning for a bus that will never be polled — inconsistent and noisy, since `build_all` excludes that channel entirely (plan.rs:85).

**Suggested fix:** Add a per-transaction margin term to `txn_ms` (a configurable or fixed conservative constant, e.g. 5 ms, standing in for the design's request_timeout_margin), and skip `estimate_bus_load` (or the whole per-channel rule body except structural checks) when `!ch.enabled`, mirroring the existing `d.enabled` filter.

---

## 24. [MINOR] [concurrency] ChangeBatch.seq is not monotonic in delivery order with more than one channel: gap detection as documented cannot work
**File:** crates/mb-poller/src/cache.rs:140-144

`fn send_batch(&self, tags: Arc<[TagId]>) { let seq = self.batch_seq.fetch_add(1, Ordering::Relaxed); let _ = self.changes.send(ChangeBatch { tags, seq }); }` — one TagCache is shared by all channel tasks (lib.rs Poller::spawn clones the same sink into every task), and `batch_seq` is documented as a 'Monotonic batch counter so subscribers can detect gaps' (cache.rs:74-75). The fetch_add and the broadcast send are not atomic together: channel A can reserve seq=5, be preempted, channel B reserves seq=6 and sends first, then A sends 5. A subscriber sees 6 then 5 — a 'gap' (5→6 skipped then rewound) that never happened, so any seq-based gap/resync logic will either false-trigger full snapshot_all() resyncs or, if it tolerates reordering, cannot distinguish real Lagged-adjacent losses. The single-threaded unit test (cache.rs:251) can't catch this.

**Suggested fix:** Assign seq and send under one short critical section (e.g. a parking_lot::Mutex around {fetch seq; send}), or drop the gap-detection claim and rely solely on broadcast's built-in Lagged signal for loss detection (which is already the documented resync trigger), removing seq from ChangeBatch or documenting it as unordered.

---

## 25. [MINOR] [quality-efficiency] Quality-flip semantics inconsistent: set_device_quality refreshes the staleness clock, degrade_uncertain doesn't; neither bumps seq
**File:** crates/mb-poller/src/cache.rs:162

`Snapshot.mono` is documented 'Monotonic; staleness math uses THIS' (cache.rs:42-43) i.e. time of last value. `set_device_quality` does `g.quality = q; g.mono = Instant::now();` (cache.rs:164-166) — so a watchdog Bad sweep makes a value that hasn't been read for minutes look 0 ms stale to any phase-3 staleness computation — while `degrade_uncertain` (cache.rs:171-177) leaves mono alone. One of these is wrong; they cannot both be right for the same 'staleness' consumer. Separately, `seq` is documented 'Bumped per value write, for change detection' (cache.rs:44-45) and neither quality method bumps it, so a phase-3 subscriber that receives a ChangeBatch and dedupes by `snapshot(tag).seq == last_seen_seq` will silently drop every quality-only transition (Good->Bad never reaches the OPC UA client). This mirrors the design pseudocode, but the internal inconsistency is real and the seq contract is a phase-3 trap.

**Suggested fix:** Stop touching mono on quality flips (keep it as time-of-last-value; quality already encodes the state), making the two methods consistent. Either bump seq on any slot mutation or explicitly document that seq is value-only and quality changes must be consumed from the ChangeBatch itself, not deduped via seq.

---

## 26. [MINOR] [concurrency] continue 'devices skips the RS-485 inter_request_delay (t3.5 silent gap) right after a timeout
**File:** crates/mb-poller/src/channel.rs:197-207

In the Timeout branch: `if !rt[dev_idx].is_online() { continue 'devices; }` jumps to the next due device, skipping the trailing `if half_duplex && !plan.inter_request_delay.is_zero() { tokio::time::sleep(plan.inter_request_delay).await; }` at channel.rs:205-207. So precisely in the worst case — a request just timed out and its late reply may still be arriving on the bus — the next device's request is sent with no silent gap. The gap after a timeout is the main mitigation that lets tokio-modbus's pre-send `read_buffer_mut().clear()` discard the late frame; skipping it maximises the stale-aliasing window of the other findings and violates the t3.5 spacing the field exists to guarantee.

**Suggested fix:** Perform the half-duplex inter_request_delay sleep before `continue 'devices` (e.g. hoist the sleep into a small helper and call it in the timeout branch before continuing), or restructure the loop so every sent request is followed by the gap regardless of outcome.

---

## 27. [MINOR] [concurrency] Write commands queued while the channel is disconnected are neither failed nor aged: submitters block unboundedly and stale writes execute after recovery
**File:** crates/mb-poller/src/channel.rs:53-74

During the connect-failure path and backoff (`backoff_or_stop`, channel.rs:357-371) the `writes` mpsc is not drained — backoff_or_stop selects only over sleep and shutdown. Queued WriteCommands (and their oneshot repliers) sit in the mpsc(64) for the entire outage; with default max_backoff_ms=30000 an outage of minutes leaves submitters awaiting `reply` with no bound and eventually blocks `writer.send().await` for all producers once the queue fills. Worse for liveness of the *plant*: when the channel finally reconnects, every stale queued command is then executed — a write submitted minutes earlier (whose OPC UA client long since timed out and may have compensated) is fired at the device with no staleness check. Nothing in the phase-1 crates bounds this; the design (§8) specifies the mpsc but no failure/TTL policy for a down channel.

**Suggested fix:** On entering the reconnect/backoff path, drain `writes` and reply `Err(ProtoError::NotConnected)` to each queued command (also select on writes.recv() inside backoff_or_stop to fail fast new arrivals), or stamp WriteCommand with a deadline and drop expired commands with an error reply before execution.

---

## 28. [MINOR] [quality-efficiency] Connect-failure path re-sweeps every tag Bad and re-notifies on every backoff attempt (no edge detection)
**File:** crates/mb-poller/src/channel.rs:66

In run_channel, every failed connect attempt runs `for d in &plan.devices { sink.set_device_quality(&d.all_tags, Quality::Bad); }` (channel.rs:66-68) unconditionally — including when all tags are already Bad from the previous attempt. Each sweep takes a write lock per tag, calls `Instant::now()` per tag (cache.rs:164-167), and emits one ChangeBatch per device into the single global 256-slot broadcast ring (cache.rs:55, shared by ALL channels). During a network outage at the stated scale (>100 devices across channels, base backoff 500 ms), the early backoff rounds flood the ring with no-op batches, evicting live batches from healthy channels and pushing subscribers into Lagged -> snapshot_all() full resyncs (5000-snapshot allocation each) exactly when the system is already stressed. The device watchdog does this correctly (on_comm_failure returns true only on the offline transition); the channel-level sweep has no equivalent edge.

**Suggested fix:** Track a `swept: bool` in run_channel: sweep on the first failed connect after a connected period, skip on subsequent attempts, reset on successful connect. Also hoist `Instant::now()` out of the per-tag loop in set_device_quality.

---

## 29. [MINOR] [concurrency] PollerHandle drop semantics are the opposite of the doc comment: dropping the handle stops all channel tasks instead of detaching them
**File:** crates/mb-poller/src/lib.rs:46-47

The doc says 'Dropping it without shutdown detaches the channel tasks (they keep polling until the runtime stops).' But PollerHandle owns `shutdown: watch::Sender<bool>`; dropping the handle drops the sender, making every receiver's `changed()` return Err, and both exit paths treat Err as stop: channel.rs:88-93 `res = shutdown.changed() => { if res.is_err() || *shutdown.borrow() { let _ = tx.disconnect().await; return; } }` and backoff_or_stop channel.rs:369 `res = shutdown.changed() => res.is_err() || *shutdown.borrow()`. So dropping the handle performs an implicit un-awaited shutdown of every channel. Anyone relying on the documented detach behavior (e.g. spawning a poller and intentionally leaking the handle in a service that keeps the runtime alive) gets a silently dead poller — all tags frozen at their last value/quality with no Bad sweep.

**Suggested fix:** Either fix the doc comment to state that dropping the handle stops the tasks, or make detach real by treating `changed() == Err` as 'sender gone, keep last value' (only stop when `*shutdown.borrow()` is true). The current stop-on-drop behavior is arguably the safer semantic — updating the doc is the minimal fix.

---

## 30. [MINOR] [quality-efficiency] Poller::spawn consumes ResolvedConfig, which cannot be cloned — phase-3 layers need it afterwards
**File:** crates/mb-poller/src/lib.rs:82

`Poller::spawn(cfg: gateway_config::ResolvedConfig, ...)` takes the config by value but only ever borrows it (`plan::build_all(&cfg)`, lib.rs:83; `TagCache::new(&cfg)` in spawn_with_cache, lib.rs:109). `ResolvedConfig` derives only Debug (resolve.rs:16-17) and can never derive Clone because `warnings: Vec<ConfigError>` holds `ConfigError::Io(std::io::Error)` (config/src/lib.rs:27). Per the design's §6 boundary, tags-core/OPC UA must read per-register metadata (data_type, word/byte order, scale, offset, tag_names) from 'the resolved config, not from the cache' — but after spawn the config is gone, forcing phase 3 to pre-extract everything into its own structures in a fragile must-run-before-spawn ordering, or to load the file twice.

**Suggested fix:** Change spawn/spawn_with_cache to take `&ResolvedConfig` (nothing in them needs ownership), or return the config in the handle. Optionally move warnings out of ResolvedConfig (return `(ResolvedConfig, Vec<ConfigError>)` from load) so the type can become Clone.

---

## 31. [MINOR] [concurrency] Per-device request_timeout_ms and retry overrides are resolved by gateway-config but silently ignored by the poller
**File:** crates/mb-poller/src/plan.rs:119

resolve.rs carefully computes per-device values: `request_timeout_ms: dev.request_timeout_ms.unwrap_or(ch.request_timeout_ms), retry: dev.retry.unwrap_or(ch.retry)` (resolve.rs:160-161), and the schema documents `request_timeout_ms: Option<u64> // overrides channel` (schema/v1.rs:235). But DevicePlan (plan.rs:37-47) carries neither field; the runtime uses only channel-level values: `DeviceRuntime::new(d.offline_after, plan.retry)` (channel.rs:48), `tx.request(dev.unit, &txn.req, plan.request_timeout)` via poll_with_retry (channel.rs:228) and service_write (channel.rs:282), and `attempt >= plan.retry.max_retries` (channel.rs:252). A mixed RS-485 bus with one slow slave configured with a longer per-device timeout will spuriously time it out (feeding the offline watchdog and, per the other findings, the half-duplex desync paths). Note the design-doc sketch of DevicePlan has the same omission, so this may be a known deviation — but the config surface actively promises the override, resolve implements it, and the poller drops it, which is a real functional bug, not a documented deviation.

**Suggested fix:** Add `request_timeout: Duration` and `retry: RetryConfig` to DevicePlan, populate them in build_device from ResolvedDevice, and use `dev.request_timeout`/`dev.retry` in poll_with_retry, service_write, and DeviceRuntime::new instead of the channel-level plan fields.

---

## 32. [MINOR] [quality-efficiency] Blocking DNS resolution on the async runtime in the reconnect path
**File:** crates/mb-proto/src/connect.rs:53

`fn resolve(host, port)` uses `std::net::ToSocketAddrs::to_socket_addrs()` (connect.rs:53-59), a synchronous, potentially seconds-long blocking call (OS resolver), executed directly on a tokio worker thread inside `Transport::connect` — which runs on every reconnect attempt of every channel. With hostname-based configs and a slow/dead DNS server during an outage, N channels reconnecting can pin all worker threads in blocking DNS, stalling healthy channels' poll loops and timers on the same runtime. The e2e tests use IP literals so this never surfaces.

**Suggested fix:** Use `tokio::net::lookup_host((host, port)).await` inside the existing connect timeout (both call sites already run under `tokio::time::timeout`), taking the first result as today.

---

## 33. [MINOR] [quality-efficiency] ProtoError::Protocol(String) erases structure and is also (mis)used for DNS resolution failure
**File:** crates/mb-proto/src/error.rs:19

`Protocol(String)` is produced by stringifying `tokio_modbus::ProtocolError` at error.rs:41 (`Err(TmErr::Protocol(p)) => Err(ProtoError::Protocol(p.to_string()))`), so phase-3 diagnostics can never distinguish HeaderMismatch from FunctionCodeMismatch except by substring matching (the crate's own test already resorts to `msg.contains("function codes")`, error.rs:88). Worse, connect.rs:58 reuses the same variant for a completely different failure: `.ok_or_else(|| ProtoError::Protocol("bad host".into()))` when DNS resolution returns no addresses — a config/network problem is reported as 'protocol/frame desync' in logs, and the variant's documented meaning ('Frame desync: HeaderMismatch / FunctionCodeMismatch', error.rs:17) plus the `protocol_errors` metric semantics ('Frame-desync / protocol errors', metrics.rs:20) become unreliable.

**Suggested fix:** Make it structured: `Protocol { kind: ProtocolKind, detail: String }` with `enum ProtocolKind { HeaderMismatch, FunctionCodeMismatch, UnexpectedResponse }` mapped in flatten(), and add a separate `ProtoError::Resolve(String)` (still is_fatal) for the connect.rs host-resolution failure.

---

