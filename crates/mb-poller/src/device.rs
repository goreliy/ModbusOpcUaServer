//! Per-device runtime state (design §7): failure counter, online flag, probe
//! backoff for offline devices, and adaptive de-coalescing (§5).
//!
//! The offline watchdog is counter-based — no extra task. Once a device
//! crosses `offline_after` consecutive comm failures it flips offline and is
//! skipped by the scheduler except on its probe cadence (exponential from
//! `base_backoff_ms` up to `max_backoff_ms`), so a dead slave cannot burn a
//! full timeout every fast-group tick and starve healthy slaves on the bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gateway_config::schema::v1::RetryConfig;
use mb_proto::{ExceptionCode, ModbusRequest};

use crate::cache::{Quality, RawValueSink};
use crate::plan::{Field, Transaction};

pub struct DeviceRuntime {
    fails: u32,
    online: bool,
    offline_after: u32,
    retry: RetryConfig,
    /// Current probe interval while offline (exponential, capped).
    probe_backoff_ms: u64,
    /// Next allowed probe while offline.
    next_probe_at: Instant,
    /// Adaptive de-coalescing (§5): `(group_idx, txn_idx)` of a coalesced
    /// transaction that returned `IllegalDataAddress` -> its per-field
    /// replacement singletons, used by every subsequent tick.
    split_overrides: HashMap<(usize, usize), Arc<[Transaction]>>,
}

impl DeviceRuntime {
    pub fn new(offline_after: u32, retry: RetryConfig) -> Self {
        Self {
            fails: 0,
            online: true,
            // offline_after == 0 would flip offline before the first request;
            // treat it as 1 ("first failure -> offline").
            offline_after: offline_after.max(1),
            retry,
            probe_backoff_ms: retry.base_backoff_ms,
            next_probe_at: Instant::now(),
            split_overrides: HashMap::new(),
        }
    }

    // NOTE (#1/#2): there is deliberately NO reset hook for channel
    // (re)connects. A fresh serial open / terminal-server accept says nothing
    // about a slave that just timed out, so `fails` / `online` /
    // `next_probe_at` survive reconnects — otherwise the half-duplex
    // consec-timeout reconnect would wipe the counter every cycle and the
    // offline watchdog could never trip. Only a successful request to THIS
    // device ([`Self::on_success`]) clears the failure state.

    /// Successful request (read, probe, write ack or even an exception reply —
    /// the slave answered): reset the failure counter, flip back online, clear
    /// the probe backoff. Returns `true` if the device just came back online.
    pub fn on_success(&mut self) -> bool {
        let recovered = !self.online;
        self.fails = 0;
        self.online = true;
        self.probe_backoff_ms = self.retry.base_backoff_ms;
        recovered
    }

    /// Comm failure (timeout / fatal) on this device. Returns `true` exactly
    /// when the device crosses the offline threshold — the caller must then
    /// sweep `all_tags` Bad via [`RawValueSink::set_device_quality`].
    pub fn on_comm_failure(&mut self) -> bool {
        self.fails = self.fails.saturating_add(1);
        if self.online {
            if self.fails >= self.offline_after {
                self.online = false;
                self.probe_backoff_ms = self.retry.base_backoff_ms;
                self.next_probe_at = Instant::now() + Duration::from_millis(self.probe_backoff_ms);
                return true;
            }
        } else {
            // Failed probe: escalate the probe cadence up to max_backoff.
            self.probe_backoff_ms = self
                .probe_backoff_ms
                .saturating_mul(2)
                .max(1)
                .min(self.retry.max_backoff_ms);
            self.next_probe_at = Instant::now() + Duration::from_millis(self.probe_backoff_ms);
        }
        false
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    /// Scheduler gate: `true` = skip this device for this tick (offline and
    /// its probe is not due yet), so a dead slave doesn't burn a timeout on
    /// every fast-group tick.
    pub fn is_offline_and_not_due_to_probe(&self) -> bool {
        !self.online && Instant::now() < self.next_probe_at
    }

    /// Current probe interval (test/diagnostics visibility).
    pub fn probe_backoff_ms(&self) -> u64 {
        self.probe_backoff_ms
    }

    /// The live replacement for `(group_idx, txn_idx)` if it was de-coalesced.
    pub fn effective_txns(&self, group_idx: usize, txn_idx: usize) -> Option<Arc<[Transaction]>> {
        self.split_overrides.get(&(group_idx, txn_idx)).cloned()
    }

    /// §5 adaptive de-coalescing. `IllegalDataAddress` on a *coalesced*
    /// transaction splits it into per-field singletons and remembers the
    /// split (subsequent ticks poll the sub-transactions); one bad bridged
    /// register can then no longer strand the whole group as Bad. Any other
    /// exception — or an exception on a simple transaction — degrades only
    /// this transaction's tags to Bad (device stays online: it answered).
    ///
    /// Returns `true` if the transaction was split.
    pub fn handle_exception(
        &mut self,
        group_idx: usize,
        txn_idx: usize,
        txn: &Transaction,
        exc: ExceptionCode,
        sink: &dyn RawValueSink,
    ) -> bool {
        if txn.coalesced
            && exc == ExceptionCode::IllegalDataAddress
            && !self.split_overrides.contains_key(&(group_idx, txn_idx))
        {
            let subs = split_txn(txn);
            tracing::warn!(
                base = txn.base,
                fields = txn.fields.len(),
                "IllegalDataAddress on coalesced read: de-coalescing into per-field requests"
            );
            self.split_overrides.insert((group_idx, txn_idx), subs);
            return true;
        }
        // Tag-scoped Bad; last value kept. Link is healthy — the slave answered.
        sink.set_device_quality(&txn.tags(), Quality::Bad);
        false
    }
}

/// Split a coalesced area read into one singleton transaction per field.
fn split_txn(txn: &Transaction) -> Arc<[Transaction]> {
    txn.fields
        .iter()
        .map(|f| {
            let addr = txn.base + f.word_offset;
            let qty = f.word_len.max(1);
            let req = match &txn.req {
                ModbusRequest::ReadCoils { .. } => ModbusRequest::ReadCoils { addr, qty },
                ModbusRequest::ReadDiscreteInputs { .. } => {
                    ModbusRequest::ReadDiscreteInputs { addr, qty }
                }
                ModbusRequest::ReadHoldingRegisters { .. } => {
                    ModbusRequest::ReadHoldingRegisters { addr, qty }
                }
                ModbusRequest::ReadInputRegisters { .. } => {
                    ModbusRequest::ReadInputRegisters { addr, qty }
                }
                // Only area reads ever coalesce (plan.rs); keep the request
                // as-is if this invariant is ever violated.
                other => other.clone(),
            };
            Transaction {
                req,
                base: addr,
                fields: vec![Field {
                    word_offset: 0,
                    ..f.clone()
                }],
                coalesced: false,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::FakeSink;
    use mb_types::{ByteOrder, DataType, TagId, WordOrder};

    fn retry(base: u64, max: u64) -> RetryConfig {
        RetryConfig {
            max_retries: 2,
            base_backoff_ms: base,
            max_backoff_ms: max,
        }
    }

    fn field(tag: u32, off: u16, len: u16) -> Field {
        Field {
            tag: TagId(tag),
            word_offset: off,
            word_len: len,
            data_type: DataType::U16,
            word_order: WordOrder::BigEndian,
            byte_order: ByteOrder::BigEndian,
            bit: None,
        }
    }

    fn coalesced_holding() -> Transaction {
        Transaction {
            req: ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 4 },
            base: 100,
            fields: vec![field(0, 0, 2), field(1, 3, 1)],
            coalesced: true,
        }
    }

    #[test]
    fn offline_threshold_and_recovery() {
        let mut rt = DeviceRuntime::new(3, retry(0, 100));
        assert!(rt.is_online());
        assert!(!rt.on_comm_failure());
        assert!(!rt.on_comm_failure());
        assert!(rt.on_comm_failure(), "third failure crosses offline_after=3");
        assert!(!rt.is_online());
        // Further failures never re-report the crossing.
        assert!(!rt.on_comm_failure());

        // Success flips back online, resets the counter and reports recovery.
        assert!(rt.on_success());
        assert!(rt.is_online());
        assert!(!rt.on_success(), "already online: no recovery event");
        assert!(!rt.on_comm_failure());
        assert!(!rt.on_comm_failure());
        assert!(rt.on_comm_failure(), "counter was reset by on_success");
    }

    #[test]
    fn probe_gating_and_backoff_escalation() {
        // Large backoff: probes are gated for a long time after going offline.
        let mut rt = DeviceRuntime::new(1, retry(10_000, 60_000));
        assert!(!rt.is_offline_and_not_due_to_probe(), "online: never gated");
        assert!(rt.on_comm_failure());
        assert!(rt.is_offline_and_not_due_to_probe(), "probe not due yet");
        assert_eq!(rt.probe_backoff_ms(), 10_000);

        // Failed probes escalate exponentially and cap at max_backoff.
        rt.on_comm_failure();
        assert_eq!(rt.probe_backoff_ms(), 20_000);
        rt.on_comm_failure();
        assert_eq!(rt.probe_backoff_ms(), 40_000);
        rt.on_comm_failure();
        assert_eq!(rt.probe_backoff_ms(), 60_000, "capped at max_backoff_ms");
        rt.on_comm_failure();
        assert_eq!(rt.probe_backoff_ms(), 60_000);

        // Zero backoff: probe due immediately (no gating).
        let mut rt0 = DeviceRuntime::new(1, retry(0, 0));
        assert!(rt0.on_comm_failure());
        assert!(!rt0.is_offline_and_not_due_to_probe());

        // Only a successful request resets the state machine.
        assert!(rt.on_success());
        assert!(rt.is_online());
        assert_eq!(rt.probe_backoff_ms(), 10_000);
        assert!(!rt.is_offline_and_not_due_to_probe());
    }

    /// #1/#2: a channel reconnect performs NO DeviceRuntime reset (there is no
    /// hook to call), so failures accumulate across reconnect cycles and the
    /// offline threshold stays reachable on half-duplex transports.
    #[test]
    fn fails_survive_reconnect_and_threshold_crosses_across_cycles() {
        let mut rt = DeviceRuntime::new(3, retry(10_000, 60_000));

        // Connection 1: two timeouts, then the channel reconnects
        // (consec-timeout stream-drain rule).
        assert!(!rt.on_comm_failure());
        assert!(!rt.on_comm_failure());
        // -- reconnect happens here: nothing is called on the runtime --
        assert!(rt.is_online(), "still online below the threshold");

        // Connection 2: the very next failure crosses offline_after=3.
        assert!(
            rt.on_comm_failure(),
            "third consecutive failure crosses the threshold across reconnect cycles"
        );
        assert!(!rt.is_online());
    }

    /// #1/#2: probe gating/backoff also survive a reconnect — a dead slave
    /// must not be re-probed at full rate just because the socket cycled.
    #[test]
    fn probe_backoff_survives_reconnect() {
        let mut rt = DeviceRuntime::new(1, retry(10_000, 60_000));
        assert!(rt.on_comm_failure());
        rt.on_comm_failure(); // failed probe: escalate
        assert_eq!(rt.probe_backoff_ms(), 20_000);
        assert!(rt.is_offline_and_not_due_to_probe());

        // -- reconnect happens here: nothing is called on the runtime --
        assert!(!rt.is_online(), "reconnect is not proof of life");
        assert!(rt.is_offline_and_not_due_to_probe(), "probe still gated");
        assert_eq!(rt.probe_backoff_ms(), 20_000, "backoff not rewound");
    }

    /// #19: the channel calls `on_success` when a slave answers with an
    /// exception — an exception reply is proof of life and must revive an
    /// offline device (reset fails, clear probe backoff, report recovery).
    #[test]
    fn exception_reply_revives_offline_device() {
        let mut rt = DeviceRuntime::new(2, retry(10_000, 60_000));
        assert!(!rt.on_comm_failure());
        assert!(rt.on_comm_failure(), "second failure crosses offline_after=2");
        rt.on_comm_failure(); // failed probe: escalate
        assert!(!rt.is_online());
        assert!(rt.is_offline_and_not_due_to_probe());
        assert_eq!(rt.probe_backoff_ms(), 20_000);

        // Exception arm: on_success first (proof of life)...
        assert!(rt.on_success(), "recovery event fires");
        assert!(rt.is_online());
        assert_eq!(rt.probe_backoff_ms(), 10_000, "probe backoff cleared");
        assert!(!rt.is_offline_and_not_due_to_probe());

        // ...then handle_exception classifies it; the device stays online.
        let sink = FakeSink::default();
        let txn = coalesced_holding();
        rt.handle_exception(0, 0, &txn, ExceptionCode::IllegalFunction, &sink);
        assert!(rt.is_online());
        // The failure counter really was reset by the revival: one new
        // failure does not cross offline_after=2 again.
        assert!(!rt.on_comm_failure());
        assert!(rt.is_online());
    }

    #[test]
    fn illegal_data_address_on_coalesced_txn_splits_and_remembers() {
        let mut rt = DeviceRuntime::new(3, retry(0, 100));
        let sink = FakeSink::default();
        let txn = coalesced_holding();

        assert!(rt.effective_txns(0, 0).is_none());
        let split = rt.handle_exception(0, 0, &txn, ExceptionCode::IllegalDataAddress, &sink);
        assert!(split);
        assert!(sink.quality_calls().is_empty(), "split does not mark tags Bad");

        let subs = rt.effective_txns(0, 0).expect("override remembered");
        assert_eq!(subs.len(), 2);
        assert!(matches!(
            subs[0].req,
            ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 2 }
        ));
        assert!(matches!(
            subs[1].req,
            ModbusRequest::ReadHoldingRegisters { addr: 103, qty: 1 }
        ));
        for s in subs.iter() {
            assert!(!s.coalesced);
            assert_eq!(s.fields.len(), 1);
            assert_eq!(s.fields[0].word_offset, 0);
        }
        assert_eq!(subs[0].fields[0].tag, TagId(0));
        assert_eq!(subs[1].fields[0].tag, TagId(1));
        // Other (group, txn) slots are untouched.
        assert!(rt.effective_txns(0, 1).is_none());
        assert!(rt.effective_txns(1, 0).is_none());

        // A second IllegalDataAddress on the same slot no longer splits: the
        // tags degrade Bad instead.
        let split = rt.handle_exception(0, 0, &txn, ExceptionCode::IllegalDataAddress, &sink);
        assert!(!split);
        assert_eq!(sink.quality_calls(), vec![(vec![TagId(0), TagId(1)], Quality::Bad)]);
    }

    #[test]
    fn other_exceptions_degrade_tags_without_splitting() {
        let mut rt = DeviceRuntime::new(3, retry(0, 100));
        let sink = FakeSink::default();

        // Coalesced + non-IllegalDataAddress: no split.
        let txn = coalesced_holding();
        assert!(!rt.handle_exception(0, 0, &txn, ExceptionCode::IllegalFunction, &sink));
        assert!(rt.effective_txns(0, 0).is_none());

        // Simple txn + IllegalDataAddress: no split either.
        let simple = Transaction {
            req: ModbusRequest::ReadHoldingRegisters { addr: 7, qty: 1 },
            base: 7,
            fields: vec![field(5, 0, 1)],
            coalesced: false,
        };
        assert!(!rt.handle_exception(0, 1, &simple, ExceptionCode::IllegalDataAddress, &sink));
        assert!(rt.effective_txns(0, 1).is_none());

        let calls = sink.quality_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1], (vec![TagId(5)], Quality::Bad));
        // Device stays online throughout: the slave answered.
        assert!(rt.is_online());
    }
}
