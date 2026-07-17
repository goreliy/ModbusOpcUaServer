//! Raw-frame traffic tee (plan §6): wraps the byte stream under the Modbus
//! codec and hex-dumps everything that crosses it to the `modbus_traffic`
//! tracing target at DEBUG level. Field diagnostics without Wireshark.
//!
//! Enabled per channel (`log_traffic: true`) and gated by the log filter
//! (e.g. `logging.level = "info,modbus_traffic=debug"`), so the hot path
//! costs nothing unless both are on.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The tracing target carrying the hex dumps.
pub const TRAFFIC_TARGET: &str = "modbus_traffic";

#[derive(Debug)]
pub(crate) struct Tee<S> {
    inner: S,
    channel: Arc<str>,
}

impl<S> Tee<S> {
    pub(crate) fn new(inner: S, channel: &str) -> Self {
        Self {
            inner,
            channel: Arc::from(channel),
        }
    }
}

/// "01 03 00 00 00 02 c4 0b"
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{b:02x}"));
    }
    out
}

impl<S: AsyncRead + Unpin> AsyncRead for Tee<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let received = &buf.filled()[before..];
            if !received.is_empty() {
                tracing::debug!(
                    target: TRAFFIC_TARGET,
                    channel = %this.channel,
                    dir = "rx",
                    bytes = received.len(),
                    frame = %hex(received),
                );
            }
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tee<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let res = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &res {
            if *n > 0 {
                tracing::debug!(
                    target: TRAFFIC_TARGET,
                    channel = %this.channel,
                    dir = "tx",
                    bytes = n,
                    frame = %hex(&buf[..*n]),
                );
            }
        }
        res
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn hex_formats_lowercase_spaced() {
        assert_eq!(hex(&[0x01, 0x03, 0x00, 0xC4, 0x0B]), "01 03 00 c4 0b");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0xFF]), "ff");
    }

    #[tokio::test]
    async fn tee_is_transparent() {
        // Whatever crosses the tee must arrive unmodified.
        let (client, mut server) = tokio::io::duplex(64);
        let mut teed = Tee::new(client, "test-ch");

        teed.write_all(&[1, 2, 3, 4]).await.unwrap();
        let mut got = [0u8; 4];
        server.read_exact(&mut got).await.unwrap();
        assert_eq!(got, [1, 2, 3, 4]);

        server.write_all(&[9, 8, 7]).await.unwrap();
        let mut got = [0u8; 3];
        teed.read_exact(&mut got).await.unwrap();
        assert_eq!(got, [9, 8, 7]);
    }
}
