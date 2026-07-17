//! `mb-proto` — Modbus transports, framing and the typed request/response layer.
//!
//! All three transports (TCP, serial RTU, RTU-over-TCP) collapse onto a single
//! `tokio_modbus::client::Context`; RTU-over-TCP is `rtu::attach_slave` over a
//! raw `TcpStream` (no hand-rolled CRC/framing). See docs/phase1-design.md §3.

pub mod connect;
pub mod error;
pub mod request;
pub mod traffic;
pub mod transport;

pub use error::{flatten, ProtoError, ProtocolKind};
pub use request::{ModbusRequest, ModbusResponse};
pub use transport::{Kind, Transport};

/// Re-exported so consumers (`mb-poller`) can classify
/// [`ProtoError::Exception`] payloads without depending on `tokio-modbus`.
pub use tokio_modbus::ExceptionCode;

/// Enumerate serial ports present on this machine (for the RTU config UI):
/// port names like `COM3` (Windows) or `/dev/ttyUSB0` (Linux), sorted and
/// deduplicated. Returns an empty list if enumeration fails (no ports, or the
/// platform refuses) — the caller keeps free-form entry as a fallback.
pub fn available_serial_ports() -> Vec<String> {
    let mut names: Vec<String> = tokio_serial::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}
