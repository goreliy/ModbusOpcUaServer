//! Per-channel poll metrics (design §7): plain `AtomicU64`, written only by
//! the owning channel task, read via relaxed loads by a future diagnostics
//! endpoint. No locks, no allocation on the hot path.

use std::sync::atomic::{AtomicU64, Ordering};

/// Counters for one channel task. Wrapped in an `Arc` shared between the
/// channel task (writer) and [`crate::PollerHandle`] (reader).
#[derive(Debug, Default)]
pub struct ChannelMetrics {
    /// Wire requests answered successfully (per attempt, reads + writes).
    pub reqs_ok: AtomicU64,
    /// Wire requests that errored (per attempt; superset of the below).
    pub reqs_err: AtomicU64,
    /// Request-level timeouts.
    pub timeouts: AtomicU64,
    /// Modbus exception responses (link healthy, slave rejected).
    pub exceptions: AtomicU64,
    /// Frame-desync / protocol errors (fatal).
    pub protocol_errors: AtomicU64,
    /// Connection drops: failed connect attempts + fatal errors that forced a
    /// `continue 'reconnect`.
    pub reconnects: AtomicU64,
    /// Write commands acknowledged.
    pub writes_ok: AtomicU64,
    /// Write commands that errored.
    pub writes_err: AtomicU64,
}

/// Point-in-time copy of [`ChannelMetrics`] (relaxed loads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    pub reqs_ok: u64,
    pub reqs_err: u64,
    pub timeouts: u64,
    pub exceptions: u64,
    pub protocol_errors: u64,
    pub reconnects: u64,
    pub writes_ok: u64,
    pub writes_err: u64,
}

impl ChannelMetrics {
    pub(crate) fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let ld = |c: &AtomicU64| c.load(Ordering::Relaxed);
        MetricsSnapshot {
            reqs_ok: ld(&self.reqs_ok),
            reqs_err: ld(&self.reqs_err),
            timeouts: ld(&self.timeouts),
            exceptions: ld(&self.exceptions),
            protocol_errors: ld(&self.protocol_errors),
            reconnects: ld(&self.reconnects),
            writes_ok: ld(&self.writes_ok),
            writes_err: ld(&self.writes_err),
        }
    }
}
