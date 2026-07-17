//! The transport enum: plain enum dispatch, no `Box<dyn>` in the hot loop.

use std::time::Duration;

use gateway_config::schema::v1::TransportConfig;
use tokio_modbus::client::Context;
use tokio_modbus::prelude::{Client, Response, Slave, SlaveContext, Writer};

use crate::{connect, error::flatten, ModbusRequest, ModbusResponse, ProtoError};

/// One connection. The channel task owns exactly one of these by value, so the
/// borrow checker enforces "one request in flight" on a half-duplex bus for free.
pub enum Transport {
    Tcp(Context),
    Rtu(Context),
    RtuOverTcp(Context),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Tcp,
    Rtu,
    RtuOverTcp,
}

impl Transport {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Tcp(_) => Kind::Tcp,
            Self::Rtu(_) => Kind::Rtu,
            Self::RtuOverTcp(_) => Kind::RtuOverTcp,
        }
    }

    pub fn is_half_duplex(&self) -> bool {
        !matches!(self, Self::Tcp(_))
    }

    /// (Re)connect. RTU-over-TCP simply attaches the RTU client to a raw
    /// `TcpStream` — verified: `rtu::attach_slave<T: AsyncRead+AsyncWrite+Unpin+Send+'static>`.
    pub async fn connect(cfg: &TransportConfig) -> Result<Self, ProtoError> {
        Self::connect_traced(cfg, None).await
    }

    /// `traffic = Some(channel_name)` hex-dumps every raw frame on this
    /// connection to the `modbus_traffic` tracing target (plan §6).
    pub async fn connect_traced(
        cfg: &TransportConfig,
        traffic: Option<&str>,
    ) -> Result<Self, ProtoError> {
        match cfg {
            TransportConfig::Tcp {
                host,
                port,
                connect_timeout_ms,
            } => Ok(Self::Tcp(
                connect::tcp(host, *port, *connect_timeout_ms, traffic).await?,
            )),
            TransportConfig::Rtu {
                path,
                baud,
                data_bits,
                parity,
                stop_bits,
            } => Ok(Self::Rtu(
                connect::rtu_serial(path, *baud, *data_bits, *parity, *stop_bits, traffic).await?,
            )),
            TransportConfig::RtuOverTcp {
                host,
                port,
                connect_timeout_ms,
            } => Ok(Self::RtuOverTcp(
                connect::rtu_over_tcp(host, *port, *connect_timeout_ms, traffic).await?,
            )),
        }
    }

    fn ctx(&mut self) -> &mut Context {
        match self {
            Self::Tcp(c) | Self::Rtu(c) | Self::RtuOverTcp(c) => c,
        }
    }

    /// Gracefully shut down the underlying stream. Consumes the transport: a
    /// disconnected `Transport` cannot be reused (reconnect via `connect`).
    pub async fn disconnect(mut self) -> Result<(), ProtoError> {
        self.ctx().disconnect().await.map_err(ProtoError::Io)
    }

    /// Issue one request to `unit`, bounded by `timeout`.
    pub async fn request(
        &mut self,
        unit: u8,
        req: &ModbusRequest,
        timeout: Duration,
    ) -> Result<ModbusResponse, ProtoError> {
        let ctx = self.ctx();
        ctx.set_slave(Slave(unit));
        let fut = async {
            use ModbusRequest as R;
            // Reads go through `Context::call` directly (NOT the `Reader` trait
            // methods): tokio-modbus 0.17 validates response payload lengths
            // with `debug_assert!` only, i.e. not at all in release builds and
            // by panicking in debug builds. `call` still checks header +
            // function code; we own the length check (release-safe) below.
            match req {
                R::ReadCoils { qty, .. } | R::ReadDiscreteInputs { qty, .. } => {
                    match flatten(ctx.call(req.to_tokio_request()).await)? {
                        Response::ReadCoils(bits) | Response::ReadDiscreteInputs(bits) => {
                            expect_bits(bits, *qty)
                        }
                        other => Err(unexpected_variant(&other)),
                    }
                }
                R::ReadHoldingRegisters { qty, .. } | R::ReadInputRegisters { qty, .. } => {
                    match flatten(ctx.call(req.to_tokio_request()).await)? {
                        Response::ReadHoldingRegisters(regs)
                        | Response::ReadInputRegisters(regs) => expect_registers(regs, *qty),
                        other => Err(unexpected_variant(&other)),
                    }
                }
                R::WriteSingleCoil { addr, value } => {
                    flatten(ctx.write_single_coil(*addr, *value).await)
                        .map(|_| ModbusResponse::WriteAck)
                }
                R::WriteSingleRegister { addr, value } => {
                    flatten(ctx.write_single_register(*addr, *value).await)
                        .map(|_| ModbusResponse::WriteAck)
                }
                R::WriteMultipleCoils { addr, values } => {
                    flatten(ctx.write_multiple_coils(*addr, values).await)
                        .map(|_| ModbusResponse::WriteAck)
                }
                R::WriteMultipleRegisters { addr, values } => {
                    flatten(ctx.write_multiple_registers(*addr, values).await)
                        .map(|_| ModbusResponse::WriteAck)
                }
                // TODO(phase2): real delimited framing for Custom on stream
                // transports (RTU / RTU-over-TCP). `expect_len` cannot be used
                // for delimiting here because tokio-modbus owns the byte
                // stream inside `Context` — its RTU codec frames unknown
                // function codes by "whatever is currently buffered", which is
                // timing-dependent on a real serial port. Doing this right
                // needs a bypass framing layer (or an upstream length hint)
                // that reads exactly 1 (addr) + 1 (fc) + expect_len + 2 (crc)
                // bytes, with the fc|0x80 exception-frame special case. Until
                // then gateway-config warns on Custom reads over serial RTU
                // and the poller checks `expect_len` post-hoc in scatter().
                R::Custom { .. } => {
                    let resp = flatten(ctx.call(req.to_tokio_request()).await)?;
                    match resp {
                        Response::Custom(_, bytes) => Ok(ModbusResponse::Raw(bytes)),
                        other => Err(unexpected_variant(&other)),
                    }
                }
            }
        };
        match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(ProtoError::Timeout),
        }
    }
}

/// Register response must carry exactly the requested quantity; anything else
/// is a stale/aliased or malformed frame -> fatal `UnexpectedResponse`.
fn expect_registers(regs: Vec<u16>, qty: u16) -> Result<ModbusResponse, ProtoError> {
    if regs.len() != qty as usize {
        return Err(ProtoError::unexpected_response(format!(
            "register response carries {} words, requested {qty}",
            regs.len()
        )));
    }
    Ok(ModbusResponse::Registers(regs))
}

/// Bit responses are padded to a byte boundary on the wire, so the decoded
/// vec may legitimately be LONGER than requested — truncate to `qty`. Shorter
/// means a short/aliased reply -> fatal `UnexpectedResponse`.
fn expect_bits(mut bits: Vec<bool>, qty: u16) -> Result<ModbusResponse, ProtoError> {
    if bits.len() < qty as usize {
        return Err(ProtoError::unexpected_response(format!(
            "bit response carries {} bits, requested {qty}",
            bits.len()
        )));
    }
    bits.truncate(qty as usize);
    Ok(ModbusResponse::Bits(bits))
}

fn unexpected_variant(resp: &Response) -> ProtoError {
    ProtoError::unexpected_response(format!(
        "response variant does not match the request: {resp:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProtocolKind;

    #[test]
    fn expect_registers_requires_exact_qty() {
        // Exact: passes through.
        match expect_registers(vec![1, 2, 3], 3).unwrap() {
            ModbusResponse::Registers(r) => assert_eq!(r, vec![1, 2, 3]),
            other => panic!("expected Registers, got {other:?}"),
        }
        // Short and long replies are both fatal UnexpectedResponse.
        for regs in [vec![1u16, 2], vec![1u16, 2, 3, 4]] {
            let e = expect_registers(regs, 3).unwrap_err();
            assert!(
                matches!(
                    &e,
                    ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }
                ),
                "got {e:?}"
            );
            assert!(e.is_fatal());
        }
    }

    #[test]
    fn expect_bits_truncates_byte_padding_but_rejects_short() {
        // Wire pads to byte boundary: 10 requested, 16 decoded -> truncated.
        match expect_bits(vec![true; 16], 10).unwrap() {
            ModbusResponse::Bits(b) => assert_eq!(b.len(), 10),
            other => panic!("expected Bits, got {other:?}"),
        }
        // Exact length passes.
        assert!(matches!(
            expect_bits(vec![false; 8], 8).unwrap(),
            ModbusResponse::Bits(b) if b.len() == 8
        ));
        // Shorter than requested: fatal.
        let e = expect_bits(vec![true; 8], 10).unwrap_err();
        assert!(matches!(
            &e,
            ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }
        ));
        assert!(e.is_fatal());
    }
}
