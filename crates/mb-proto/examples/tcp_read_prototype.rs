//! Phase 0 prototype: confirms the Modbus TCP stack works end-to-end without real hardware.
//!
//! Spins up an in-process Modbus TCP slave (simulator) with a handful of holding registers,
//! then connects a client to it and reads them back. Run with:
//!   cargo run -p mb-proto --example tcp_read_prototype

use std::{
    collections::HashMap,
    future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::net::TcpListener;
use tokio_modbus::{
    prelude::*,
    server::tcp::{accept_tcp_connection, Server},
};

struct SimulatedSlave {
    holding_registers: Arc<Mutex<HashMap<u16, u16>>>,
}

impl SimulatedSlave {
    fn new() -> Self {
        let mut holding_registers = HashMap::new();
        // Pretend these came from a real device: e.g. a pump speed and a temperature.
        holding_registers.insert(0, 1500); // Pump1.Speed (raw)
        holding_registers.insert(1, 237); // Pump1.Temperature (raw, x0.1 degC)
        Self {
            holding_registers: Arc::new(Mutex::new(holding_registers)),
        }
    }
}

impl tokio_modbus::server::Service for SimulatedSlave {
    type Request = Request<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future = future::Ready<Result<Self::Response, Self::Exception>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let res = match req {
            Request::ReadHoldingRegisters(addr, cnt) => {
                let registers = self.holding_registers.lock().unwrap();
                let mut values = Vec::with_capacity(cnt as usize);
                let mut ok = true;
                for i in 0..cnt {
                    match registers.get(&(addr + i)) {
                        Some(v) => values.push(*v),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    Ok(Response::ReadHoldingRegisters(values))
                } else {
                    Err(ExceptionCode::IllegalDataAddress)
                }
            }
            _ => Err(ExceptionCode::IllegalFunction),
        };
        future::ready(res)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let socket_addr: SocketAddr = "127.0.0.1:15502".parse().unwrap();

    tokio::select! {
        _ = run_simulator(socket_addr) => unreachable!(),
        result = run_client(socket_addr) => result?,
    }

    Ok(())
}

async fn run_simulator(socket_addr: SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(socket_addr).await?;
    let server = Server::new(listener);
    let new_service = |_socket_addr| Ok(Some(SimulatedSlave::new()));
    let on_connected =
        |stream, socket_addr| async move { accept_tcp_connection(stream, socket_addr, new_service) };
    let on_process_error = |err| eprintln!("simulator error: {err}");
    server.serve(&on_connected, on_process_error).await?;
    Ok(())
}

async fn run_client(socket_addr: SocketAddr) -> anyhow::Result<()> {
    // Give the simulator a moment to start listening.
    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("Connecting Modbus TCP client to {socket_addr}...");
    let mut ctx = tcp::connect(socket_addr).await?;

    let raw = ctx.read_holding_registers(0, 2).await??;
    let speed = raw[0] as f64;
    let temperature = raw[1] as f64 * 0.1;

    println!("Pump1.Speed (raw {}) -> {} rpm", raw[0], speed);
    println!("Pump1.Temperature (raw {}) -> {:.1} degC", raw[1], temperature);

    ctx.disconnect().await?;
    Ok(())
}
