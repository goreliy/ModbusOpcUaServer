//! In-process multi-slave Modbus TCP simulator + cache helpers, shared by
//! mb-poller's integration tests and examples.
//!
//! Both `tests/*.rs` and `examples/*.rs` compile with dev-dependencies, so
//! they include this file directly:
//!
//! ```ignore
//! #[path = "../testsupport/sim.rs"]
//! mod sim;
//! ```
//!
//! The server dispatches on the MBAP **unit id** (`SlaveRequest` service), so
//! any number of "devices" live behind one listener. Register banks are
//! shared `Arc<Mutex<..>>` state that survives server restarts, letting a
//! test kill the server and bring it back with device memory intact.
#![allow(dead_code)] // included from several targets; not all use every item

use std::collections::HashMap;
use std::future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mb_poller::{CacheReader, Quality, RawValue, Snapshot, TagCache};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_modbus::{
    prelude::{ExceptionCode, Request, Response},
    server::tcp::{accept_tcp_connection, Server},
    SlaveRequest,
};

/// Per-unit-id holding register banks, shared across server restarts.
pub type Banks = Arc<Mutex<HashMap<u8, HashMap<u16, u16>>>>;

/// Dispatches on the MBAP unit id: N "devices" behind one listener.
#[derive(Clone)]
pub struct MultiSlave {
    pub banks: Banks,
}

impl tokio_modbus::server::Service for MultiSlave {
    type Request = SlaveRequest<'static>;
    type Response = Option<Response>;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let SlaveRequest { slave, request } = req;
        let mut banks = self.banks.lock().unwrap();
        let Some(bank) = banks.get_mut(&slave) else {
            return future::ready(Err(ExceptionCode::GatewayTargetDevice));
        };
        let res = match request {
            Request::ReadHoldingRegisters(addr, cnt) => (0..cnt)
                .map(|i| bank.get(&(addr + i)).copied())
                .collect::<Option<Vec<u16>>>()
                .map(|regs| Some(Response::ReadHoldingRegisters(regs)))
                .ok_or(ExceptionCode::IllegalDataAddress),
            Request::WriteSingleRegister(addr, value) => {
                bank.insert(addr, value);
                Ok(Some(Response::WriteSingleRegister(addr, value)))
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

/// Spawn the server on an already-bound listener. Aborting the returned task
/// closes the listener AND cancels every per-connection loop (tokio-modbus
/// guards them with a `CancellationToken` drop guard).
pub fn spawn_server(listener: TcpListener, banks: Banks) -> JoinHandle<()> {
    tokio::spawn(async move {
        let server = Server::new(listener);
        let service = MultiSlave { banks };
        let on_connected = move |stream, socket_addr| {
            let service = service.clone();
            async move {
                accept_tcp_connection(stream, socket_addr, move |_| Ok(Some(service.clone())))
            }
        };
        let _ = server
            .serve(&on_connected, |err| eprintln!("server error: {err}"))
            .await;
    })
}

/// Rebind a specific address; the port may linger briefly after the previous
/// listener was aborted, so retry for up to 3 s.
pub async fn bind(addr: SocketAddr) -> TcpListener {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        match TcpListener::bind(addr).await {
            Ok(l) => return l,
            Err(e) if tokio::time::Instant::now() < deadline => {
                eprintln!("rebind {addr}: {e}, retrying");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("rebind {addr}: {e}"),
        }
    }
}

/// Snapshot a tag by name (panics on unknown names — test convenience).
pub fn snap(cache: &TagCache, name: &str) -> Snapshot {
    let id = cache
        .resolve(name)
        .unwrap_or_else(|| panic!("unknown tag {name}"));
    cache.snapshot(id).expect("slot exists")
}

/// True when `name` is Good and holds exactly the single register `expect`.
pub fn good_u16(cache: &TagCache, name: &str, expect: u16) -> bool {
    let s = snap(cache, name);
    s.quality == Quality::Good && s.value == RawValue::Registers(vec![expect].into())
}

/// Poll `cond` every 20 ms until true or panic after `timeout`.
pub async fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
