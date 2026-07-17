//! Opens the byte pipe per transport. All three collapse onto a single
//! `tokio_modbus::client::Context`:
//! - TCP        -> `tcp::connect` (MBAP framing)
//! - RTU        -> `tokio_serial::SerialStream` -> `rtu::attach_slave`
//! - RTU/TCP    -> raw `TcpStream` -> `rtu::attach_slave` (verified: `attach_slave`
//!   is generic over any `AsyncRead + AsyncWrite + Unpin + Send + 'static`)

use std::time::Duration;

use gateway_config::schema::v1::Parity;
use tokio::net::TcpStream;
use tokio_modbus::client::{rtu, tcp, Context};
use tokio_modbus::prelude::Slave;

use crate::traffic::Tee;
use crate::ProtoError;

/// `traffic = Some(channel_name)` wraps the byte stream in the hex-dump tee
/// (see [`crate::traffic`]).
pub async fn tcp(
    host: &str,
    port: u16,
    timeout_ms: u64,
    traffic: Option<&str>,
) -> Result<Context, ProtoError> {
    // DNS + connect share one deadline; resolution is async (tokio's resolver
    // thread pool), never blocking a runtime worker on the OS resolver.
    let stream = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let addr = resolve(host, port).await?;
        TcpStream::connect(addr).await.map_err(ProtoError::Io)
    })
    .await
    .map_err(|_| ProtoError::Timeout)??;
    stream.set_nodelay(true).map_err(ProtoError::Io)?;
    Ok(match traffic {
        Some(ch) => tcp::attach_slave(Tee::new(stream, ch), Slave(0)),
        None => tcp::attach_slave(stream, Slave(0)),
    })
}

/// RTU-over-TCP: raw RTU frames on a plain `TcpStream`, NO MBAP — inherits the
/// battle-tested `ClientCodec` CRC + resync for free.
pub async fn rtu_over_tcp(
    host: &str,
    port: u16,
    timeout_ms: u64,
    traffic: Option<&str>,
) -> Result<Context, ProtoError> {
    let stream = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let addr = resolve(host, port).await?;
        TcpStream::connect(addr).await.map_err(ProtoError::Io)
    })
    .await
    .map_err(|_| ProtoError::Timeout)??;
    // slave overwritten per-request via set_slave
    Ok(match traffic {
        Some(ch) => rtu::attach_slave(Tee::new(stream, ch), Slave(0)),
        None => rtu::attach_slave(stream, Slave(0)),
    })
}

pub async fn rtu_serial(
    path: &str,
    baud: u32,
    data_bits: u8,
    parity: Parity,
    stop_bits: u8,
    traffic: Option<&str>,
) -> Result<Context, ProtoError> {
    let builder = tokio_serial::new(path, baud)
        .data_bits(map_data_bits(data_bits))
        .parity(map_parity(parity))
        .stop_bits(map_stop_bits(stop_bits));
    let stream =
        tokio_serial::SerialStream::open(&builder).map_err(|e| ProtoError::Io(e.into()))?;
    Ok(match traffic {
        Some(ch) => rtu::attach_slave(Tee::new(stream, ch), Slave(0)),
        None => rtu::attach_slave(stream, Slave(0)),
    })
}

async fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr, ProtoError> {
    tokio::net::lookup_host((host, port))
        .await
        .map_err(ProtoError::Io)?
        .next()
        .ok_or_else(|| ProtoError::Resolve(format!("host `{host}` resolved to no addresses")))
}

fn map_data_bits(bits: u8) -> tokio_serial::DataBits {
    match bits {
        5 => tokio_serial::DataBits::Five,
        6 => tokio_serial::DataBits::Six,
        7 => tokio_serial::DataBits::Seven,
        _ => tokio_serial::DataBits::Eight, // validate.rs restricts to 5..=8
    }
}

fn map_parity(parity: Parity) -> tokio_serial::Parity {
    match parity {
        Parity::None => tokio_serial::Parity::None,
        Parity::Even => tokio_serial::Parity::Even,
        Parity::Odd => tokio_serial::Parity::Odd,
    }
}

fn map_stop_bits(bits: u8) -> tokio_serial::StopBits {
    match bits {
        2 => tokio_serial::StopBits::Two,
        _ => tokio_serial::StopBits::One, // validate.rs restricts to 1..=2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_parameter_mapping() {
        assert_eq!(map_data_bits(5), tokio_serial::DataBits::Five);
        assert_eq!(map_data_bits(6), tokio_serial::DataBits::Six);
        assert_eq!(map_data_bits(7), tokio_serial::DataBits::Seven);
        assert_eq!(map_data_bits(8), tokio_serial::DataBits::Eight);
        assert_eq!(map_parity(Parity::None), tokio_serial::Parity::None);
        assert_eq!(map_parity(Parity::Even), tokio_serial::Parity::Even);
        assert_eq!(map_parity(Parity::Odd), tokio_serial::Parity::Odd);
        assert_eq!(map_stop_bits(1), tokio_serial::StopBits::One);
        assert_eq!(map_stop_bits(2), tokio_serial::StopBits::Two);
    }
}
