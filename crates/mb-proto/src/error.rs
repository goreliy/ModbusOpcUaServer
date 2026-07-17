//! `ProtoError` + the single site that flattens `tokio_modbus::Result<T>`.

use tokio_modbus::ExceptionCode;

/// What exactly went wrong at the protocol/framing level. Structured so
/// phase-3 diagnostics can classify without substring-matching messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKind {
    /// Response header (MBAP transaction/unit id, RTU slave id) doesn't match
    /// the request.
    HeaderMismatch,
    /// Response function code doesn't match the request.
    FunctionCodeMismatch,
    /// Response shape or payload length contradicts the request (wrong
    /// variant, or a register/bit count that doesn't match the requested qty).
    UnexpectedResponse,
}

impl std::fmt::Display for ProtocolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::HeaderMismatch => "header mismatch",
            Self::FunctionCodeMismatch => "function code mismatch",
            Self::UnexpectedResponse => "unexpected response",
        };
        f.write_str(s)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    /// Underlying I/O: connect refused, reset, EOF. Fatal -> reconnect.
    #[error("io: {0}")]
    Io(std::io::Error),
    /// Request-level timeout (our `tokio::time::timeout`). See design §6 for the
    /// stream drain rule.
    #[error("timeout")]
    Timeout,
    /// Slave answered with a Modbus exception. Link is HEALTHY. Tag-scoped Bad.
    #[error("modbus exception: {0:?}")]
    Exception(ExceptionCode),
    /// Frame desync / response contradiction. Fatal -> reconnect.
    #[error("protocol/frame desync ({kind}): {detail}")]
    Protocol { kind: ProtocolKind, detail: String },
    /// Host name resolution failed (config/DNS problem, not a frame desync).
    /// Fatal -> reconnect with backoff.
    #[error("host resolution failed: {0}")]
    Resolve(String),
    /// Transport not connected yet.
    #[error("not connected")]
    NotConnected,
}

impl ProtoError {
    /// Fatal = drop the connection and reconnect with backoff.
    /// Non-fatal (`Exception`, `Timeout`) = per-request retry on the same connection.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::Io(_) | Self::Protocol { .. } | Self::Resolve(_) | Self::NotConnected
        )
    }

    /// Shorthand for [`ProtoError::Protocol`] with
    /// [`ProtocolKind::UnexpectedResponse`].
    pub fn unexpected_response(detail: impl Into<String>) -> Self {
        Self::Protocol {
            kind: ProtocolKind::UnexpectedResponse,
            detail: detail.into(),
        }
    }
}

/// THE single flattening site. `tokio_modbus::Result<T> = Result<Result<T, ExceptionCode>, Error>`
/// where `Error = Protocol(ProtocolError) | Transport(io::Error)` (verified src/error.rs).
pub fn flatten<T>(r: tokio_modbus::Result<T>) -> Result<T, ProtoError> {
    use tokio_modbus::Error as TmErr;
    use tokio_modbus::ProtocolError as TmProto;
    match r {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(exc)) => Err(ProtoError::Exception(exc)), // slave alive, rejected
        Err(TmErr::Transport(e)) => Err(ProtoError::Io(e)), // socket/serial failure
        Err(TmErr::Protocol(p)) => Err(ProtoError::Protocol {
            kind: match &p {
                TmProto::HeaderMismatch { .. } => ProtocolKind::HeaderMismatch,
                TmProto::FunctionCodeMismatch { .. } => ProtocolKind::FunctionCodeMismatch,
            },
            detail: p.to_string(),
        }), // frame desync -> fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_modbus::{
        Error as TmErr, FunctionCode as TmFc, ProtocolError, Response,
    };

    #[test]
    fn flatten_ok_passes_value_through() {
        let r: tokio_modbus::Result<Vec<u16>> = Ok(Ok(vec![1, 2, 3]));
        assert_eq!(flatten(r).unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn flatten_exception_is_tag_scoped_and_non_fatal() {
        let r: tokio_modbus::Result<Vec<u16>> = Ok(Err(ExceptionCode::IllegalDataAddress));
        let e = flatten(r).unwrap_err();
        assert!(matches!(
            e,
            ProtoError::Exception(ExceptionCode::IllegalDataAddress)
        ));
        assert!(!e.is_fatal(), "exception means the link is healthy");
    }

    #[test]
    fn flatten_transport_error_is_fatal_io() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "peer reset");
        let r: tokio_modbus::Result<Vec<u16>> = Err(TmErr::Transport(io));
        let e = flatten(r).unwrap_err();
        assert!(matches!(e, ProtoError::Io(_)));
        assert!(e.is_fatal());
    }

    #[test]
    fn flatten_function_code_mismatch_is_fatal_and_structured() {
        let r: tokio_modbus::Result<Vec<u16>> =
            Err(TmErr::Protocol(ProtocolError::FunctionCodeMismatch {
                request: TmFc::ReadHoldingRegisters,
                result: Ok(Response::ReadCoils(vec![])),
            }));
        let e = flatten(r).unwrap_err();
        match &e {
            ProtoError::Protocol { kind, detail } => {
                assert_eq!(*kind, ProtocolKind::FunctionCodeMismatch);
                assert!(!detail.is_empty(), "detail carries the tokio-modbus message");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
        assert!(e.is_fatal());
    }

    #[test]
    fn flatten_header_mismatch_is_fatal_and_structured() {
        let r: tokio_modbus::Result<Vec<u16>> =
            Err(TmErr::Protocol(ProtocolError::HeaderMismatch {
                message: "transaction id mismatch".into(),
                result: Ok(Response::ReadHoldingRegisters(vec![])),
            }));
        let e = flatten(r).unwrap_err();
        match &e {
            ProtoError::Protocol { kind, .. } => {
                assert_eq!(*kind, ProtocolKind::HeaderMismatch);
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
        assert!(e.is_fatal());
    }

    #[test]
    fn resolve_failure_is_fatal_but_not_a_protocol_error() {
        let e = ProtoError::Resolve("no addresses for host".into());
        assert!(e.is_fatal());
        assert!(!matches!(e, ProtoError::Protocol { .. }));
    }

    #[test]
    fn timeout_is_non_fatal_and_not_connected_is_fatal() {
        assert!(!ProtoError::Timeout.is_fatal());
        assert!(ProtoError::NotConnected.is_fatal());
    }
}
