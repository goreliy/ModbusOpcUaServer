//! Integration: `Transport::RtuOverTcp` against a fake "serial tunnel".
//!
//! The tunnel is a TCP listener that speaks raw RTU frames (slave addr + PDU +
//! CRC16, no MBAP). The server side reuses tokio-modbus's own RTU codec over a
//! TcpStream via the `rtu-over-tcp-server` feature (`server::rtu_over_tcp`),
//! which verifies the client's CRC and framing on every exchange — so this
//! round-trip proves the client really emits RTU frames on the socket.

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
    server::rtu_over_tcp::{accept_tcp_connection, Server},
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

async fn spawn_rtu_tunnel() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let slave = SimSlave {
            holding: Arc::new(Mutex::new(HashMap::from([(100, 0xCAFE), (101, 0x0042)]))),
        };
        let new_service = move |_addr| Ok(Some(slave.clone()));
        let on_connected = move |stream, socket_addr| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, socket_addr, new_service) }
        };
        let _ = server.serve(&on_connected, |err| eprintln!("tunnel error: {err}")).await;
    });
    addr
}

#[tokio::test]
async fn rtu_over_tcp_round_trip() {
    let addr = spawn_rtu_tunnel().await;
    let cfg = TransportConfig::RtuOverTcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        connect_timeout_ms: 5000,
    };

    let mut tx = Transport::connect(&cfg).await.expect("connect");
    assert_eq!(tx.kind(), Kind::RtuOverTcp);
    assert!(tx.is_half_duplex(), "RTU-over-TCP tunnels a half-duplex bus");

    let timeout = Duration::from_secs(2);

    // Canned ReadHoldingRegisters exchange over raw RTU framing (CRC verified
    // by the server-side codec; response CRC verified by the client codec).
    let resp = tx
        .request(7, &ModbusRequest::ReadHoldingRegisters { addr: 100, qty: 2 }, timeout)
        .await
        .expect("rtu-over-tcp read");
    match resp {
        ModbusResponse::Registers(regs) => assert_eq!(regs, vec![0xCAFE, 0x0042]),
        other => panic!("expected Registers, got {other:?}"),
    }

    // A second exchange on the same connection (framing stayed in sync).
    let resp = tx
        .request(7, &ModbusRequest::WriteSingleRegister { addr: 100, value: 5 }, timeout)
        .await
        .expect("rtu-over-tcp write");
    assert!(matches!(resp, ModbusResponse::WriteAck));

    // Exception classification survives the RTU framing too.
    let err = tx
        .request(7, &ModbusRequest::ReadHoldingRegisters { addr: 9000, qty: 1 }, timeout)
        .await
        .expect_err("exception expected");
    assert!(matches!(err, ProtoError::Exception(ExceptionCode::IllegalDataAddress)));
    assert!(!err.is_fatal());

    tx.disconnect().await.expect("disconnect");
}

/// A slave whose register replies are always one word short of the request.
#[derive(Clone)]
struct ShortSlave;

impl tokio_modbus::server::Service for ShortSlave {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::ReadHoldingRegisters(_, cnt) => {
                Ok(Response::ReadHoldingRegisters(vec![9; (cnt - 1) as usize]))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

/// Transport-level length validation must hold on the RTU (stream) framing
/// too: a short reply is a fatal `Protocol { kind: UnexpectedResponse }`.
#[tokio::test]
async fn rtu_over_tcp_short_register_reply_is_fatal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let server = Server::new(listener);
        let new_service = move |_addr| Ok(Some(ShortSlave));
        let on_connected = move |stream, socket_addr| async move {
            accept_tcp_connection(stream, socket_addr, new_service)
        };
        let _ = server.serve(&on_connected, |err| eprintln!("tunnel error: {err}")).await;
    });

    let cfg = TransportConfig::RtuOverTcp {
        host: addr.ip().to_string(),
        port: addr.port(),
        connect_timeout_ms: 5000,
    };
    let mut tx = Transport::connect(&cfg).await.expect("connect");
    let err = tx
        .request(7, &ModbusRequest::ReadHoldingRegisters { addr: 0, qty: 4 }, Duration::from_secs(2))
        .await
        .expect_err("short reply must be rejected");
    assert!(
        matches!(&err, ProtoError::Protocol { kind: ProtocolKind::UnexpectedResponse, .. }),
        "got {err:?}"
    );
    assert!(err.is_fatal());
}
