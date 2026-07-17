//! Write path (design §8): writes (FC05/06/15/16 + Custom) flow **into** the
//! owning channel task over a per-channel bounded `mpsc(64)` and are drained
//! at a `select!` boundary *and* at every transaction boundary inside a poll
//! tick — i.e. only *between* whole transactions, never inside a coalesced
//! read burst, so two frames can never collide on RS-485, while operator
//! write latency stays bounded by ~one transaction. The submitter gets a
//! `oneshot` result per write; while the channel is disconnected or backing
//! off, queued and newly arriving writes fail fast with
//! [`ProtoError::NotConnected`] instead of firing stale at the plant later.

use std::time::Instant;

use mb_proto::{ModbusRequest, ModbusResponse, ProtoError};
use mb_types::DeviceId;
use tokio::sync::oneshot;

pub struct WriteCommand {
    /// Target device identity (#14: keyed by [`DeviceId`], not a positional
    /// index — the plan's device vec is enabled-filtered, so a raw index is a
    /// wrong-actuator hazard). The channel task resolves it against its plan
    /// and replies with an error for an unknown/disabled device.
    pub device: DeviceId,
    /// `WriteSingle*` / `WriteMultiple*` / `Custom`.
    pub req: ModbusRequest,
    pub reply: oneshot::Sender<Result<ModbusResponse, ProtoError>>,
    /// Drop-dead deadline (B1). A command dequeued after this instant is NOT
    /// sent to the device; the submitter gets [`ProtoError::Timeout`]. The
    /// submitter stops waiting at the SAME instant, so a write whose client
    /// already received a timeout can never fire late (double-actuation).
    pub deadline: Instant,
}
