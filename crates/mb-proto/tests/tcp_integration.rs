//! Integration: `Transport::Tcp` against an in-process tokio-modbus TCP server.
//!
//! Covers ReadHoldingRegisters, WriteSingleRegister and the ProtoError
//! classification of a slave-side Modbus exception (link stays healthy).

use std::{
    collections::HashMap,
    future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use gateway_config::schema::v1::TransportConfig;
use mb_proto::{Kind, ModbusRequest, ModbusResponse, ProtoError, ProtocolKind, Transport};
use tokio::net::TcpListener;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::tcp::{accept_tcp_connection, Server},
};

#[derive(Clone)]
struct SimSlave {
    holding: Arc<Mutex<HashMap<u16, u16>>>,
}

impl tokio_modbus::server::Service for SimSlave {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::ReadHoldingRegisters(addr, cnt) => {
                let regs = self.holding.lock().unwrap();
                (0..cnt)
                    .map(|i| regs.get(&(addr + i)).copied())
                    .collect::<Option<Vec<u16>>>()
                    .map(Response::ReadHoldingRegisters)
                    .ok_or(ExceptionCode::IllegalDataAddress)
            }
            Request::WriteSingleRegister(addr, value) => {
                self.holding.lock().unwrap().insert(addr, value);
                Ok(Response::WriteSingleRegister(addr, value))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

async fn spawn_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let slave = SimSlave {
            holding: Arc::new(Mutex::new(HashMap::from([(0, 1500), (1, 237)]))),
        };
        let new_service = move |_addr| Ok(Some(slave.clone()));
        let on_connected = move |stream, socket_addr| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, socket_addr, new_service) }
        };
        let _ = server.serve(&on_connected, |err| eprintln!("server error: {err}")).await;
    });
    addr
}

/// A slave whose replies never match the requested quantity: registers come
/// back one short (or one long for input registers), coil replies always
/// carry a single data byte (= 8 decoded bits).
#[derive(Clone)]
struct LyingSlave;

impl tokio_modbus::server::Service for LyingSlave {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::ReadHoldingRegisters(_, cnt) => {
                Ok(Response::ReadHoldingRegisters(vec![7; (cnt - 1) as usize]))
            }
            Request::ReadInputRegisters(_, cnt) => {
                Ok(Response::ReadInputRegisters(vec![7; (cnt + 1) as usize]))
            }
            Request::ReadCoils(_, _) => Ok(Response::ReadCoils(vec![true])),
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

async fn spawn_lying_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let new_service = move |_addr| Ok(Some(LyingSlave));
        let on_connected = move |stream, socket_addr| async move {
            accept_tcp_connection(stream, socket_addr, new_service)
        };
        let _ = server.serve(&on_connected, |err| eprintln!("server error: {err}")).await;
    });
    addr
}

/// Transport-level response-length validation (findings #6/#12, option b):
/// a payload that contradicts the requested qty is a fatal
/// `Protocol { kind: UnexpectedResponse }`, never silently published.
#[tokio::test]
async fn wrong_length_replies_are_fatal_unexpected_response() {
    let addr = spawn_lying_server().await;
    let cfg = TransportConfig::Tcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        connect_timeout_ms: 5000,
    };
    let timeout = Duration::from_secs(2);

    // Short register reply (qty-1 words).
    let mut tx = Transport::connect(&cfg).await.expect("connect");
    let err = tx
        .request(1, &ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 4 }, timeout)
        .await
        .expect_err("short register reply must be rejected");
    assert!(
        matches!(&err, ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }),
        "got {err:?}"
    );
    assert!(err.is_fatal());
    drop(tx);

    // Long register reply (qty+1 words).
    let mut tx = Transport::connect(&cfg).await.expect("connect");
    let err = tx
        .request(1, &ModbusRequest::ReadInputRegisters { addr: 0, qty: 4 }, timeout)
        .await
        .expect_err("long register reply must be rejected");
    assert!(
        matches!(&err, ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }),
        "got {err:?}"
    );
    drop(tx);

    // Coil reply with too few data bits (1 byte = 8 bits < 16 requested).
    let mut tx = Transport::connect(&cfg).await.expect("connect");
    let err = tx
        .request(1, &ModbusRequest::ReadCoils { addr: 0, qty: 16 }, timeout)
        .await
        .expect_err("short coil reply must be rejected");
    assert!(
        matches!(&err, ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }),
        "got {err:?}"
    );
    drop(tx);

    // Byte-boundary padding is NOT an error: 3 coils requested, 1 data byte
    // (8 bits) on the wire -> truncated to exactly 3 bits.
    let mut tx = Transport::connect(&cfg).await.expect("connect");
    let resp = tx
        .request(1, &ModbusRequest::ReadCoils { addr: 0, qty: 3 }, timeout)
        .await
        .expect("padded coil reply is legitimate");
    match resp {
        ModbusResponse::Bits(bits) => assert_eq!(bits.len(), 3),
        other => panic!("expected Bits, got {other:?}"),
    }
    tx.disconnect().await.expect("disconnect");
}

/// Echoes Custom requests back: response payload = request payload followed
/// by one marker byte, so the test can prove the bytes crossed the wire.
#[derive(Clone)]
struct CustomEcho;

impl tokio_modbus::server::Service for CustomEcho {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::Custom(code, data) => {
                let mut payload = data.into_owned();
                payload.push(0xEE);
                Ok(Response::Custom(code, payload.into()))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

/// Finding #16c: the Custom (vendor raw-PDU) path has wire coverage — over
/// MBAP/TCP the frame is length-prefixed, so Custom is fully reliable here.
/// `expect_len` deliberately does NOT gate the transport (the poller's
/// `scatter` enforces it post-hoc, unit-tested in channel.rs): the transport
/// returns whatever the slave sent.
#[tokio::test]
async fn custom_function_round_trips_over_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let new_service = move |_addr| Ok(Some(CustomEcho));
        let on_connected = move |stream, socket_addr| async move {
            accept_tcp_connection(stream, socket_addr, new_service)
        };
        let _ = server.serve(&on_connected, |err| eprintln!("server error: {err}")).await;
    });

    let cfg = TransportConfig::Tcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        connect_timeout_ms: 5000,
    };
    let timeout = Duration::from_secs(2);
    let mut tx = Transport::connect(&cfg).await.expect("connect");

    // Round-trip: 3 request bytes -> 4 response bytes (echo + marker).
    let resp = tx
        .request(
            1,
            &ModbusRequest::Custom { code: 0x41, data: vec![1, 2, 3], expect_len: Some(4) },
            timeout,
        )
        .await
        .expect("custom round-trip");
    match resp {
        ModbusResponse::Raw(bytes) => assert_eq!(bytes.as_ref(), &[1, 2, 3, 0xEE]),
        other => panic!("expected Raw, got {other:?}"),
    }

    // A reply that contradicts expect_len still ARRIVES as Raw — the length
    // contract is enforced by the poller (scatter -> tags Bad), not here.
    let resp = tx
        .request(
            1,
            &ModbusRequest::Custom { code: 0x42, data: vec![9], expect_len: Some(10) },
            timeout,
        )
        .await
        .expect("custom short-reply still frames");
    match resp {
        ModbusResponse::Raw(bytes) => {
            assert_eq!(bytes.as_ref(), &[9, 0xEE]);
            assert_ne!(bytes.len(), 10, "poller-side expect_len check would flag this");
        }
        other => panic!("expected Raw, got {other:?}"),
    }

    tx.disconnect().await.expect("disconnect");
}

#[tokio::test]
async fn tcp_read_write_and_exception_classification() {
    let addr = spawn_server().await;
    let cfg = TransportConfig::Tcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        connect_timeout_ms: 5000,
    };

    let mut tx = Transport::connect(&cfg).await.expect("connect");
    assert_eq!(tx.kind(), Kind::Tcp);
    assert!(!tx.is_half_duplex());

    let timeout = Duration::from_secs(2);

    // Read two known holding registers.
    let resp = tx
        .request(1, &ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 2 }, timeout)
        .await
        .expect("read");
    match resp {
        ModbusResponse::Registers(regs) => assert_eq!(regs, vec![1500, 237]),
        other => panic!("expected Registers, got {other:?}"),
    }

    // Write, then read back.
    let resp = tx
        .request(1, &ModbusRequest::WriteSingleRegister { addr: 0, value: 1600 }, timeout)
        .await
        .expect("write");
    assert!(matches!(resp, ModbusResponse::WriteAck));
    let resp = tx
        .request(1, &ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 1 }, timeout)
        .await
        .expect("read back");
    match resp {
        ModbusResponse::Registers(regs) => assert_eq!(regs, vec![1600]),
        other => panic!("expected Registers, got {other:?}"),
    }

    // Unmapped address -> Modbus exception: tag-scoped, NON-fatal (link healthy).
    let err = tx
        .request(1, &ModbusRequest::ReadHoldingRegisters { addr: 900, qty: 1 }, timeout)
        .await
        .expect_err("exception expected");
    assert!(matches!(err, ProtoError::Exception(ExceptionCode::IllegalDataAddress)));
    assert!(!err.is_fatal());

    // Unsupported function -> IllegalFunction exception, still non-fatal.
    let err = tx
        .request(1, &ModbusRequest::ReadCoils { addr: 0, qty: 1 }, timeout)
        .await
        .expect_err("exception expected");
    assert!(matches!(err, ProtoError::Exception(ExceptionCode::IllegalFunction)));
    assert!(!err.is_fatal());

    // The link is still usable after exceptions.
    let resp = tx
        .request(1, &ModbusRequest::ReadHoldingRegisters { addr: 1, qty: 1 }, timeout)
        .await
        .expect("read after exception");
    assert!(matches!(resp, ModbusResponse::Registers(regs) if regs == vec![237]));

    tx.disconnect().await.expect("disconnect");
}
