use super::{
    fingerprint, now_ms, random_id, read_file,
    tools::{number, string},
    validate, Controller,
};
use gateway_config::{schema::v1::TransportConfig, ResolvedConfig};
use mb_proto::{ModbusRequest, ModbusResponse, ProtoError, Transport};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub(super) struct ScanJob {
    pub offline: bool,
    pub cancel: AtomicBool,
    pub task: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    data: parking_lot::Mutex<Value>,
    resources: HashSet<String>,
}
impl ScanJob {
    pub fn running(&self) -> bool {
        self.data.lock()["status"] == "running"
    }
}
#[derive(Clone)]
enum Access {
    Queued {
        sender: tokio::sync::mpsc::Sender<mb_poller::WriteCommand>,
        transport: TransportConfig,
    },
    Offline(TransportConfig),
}
fn resource(t: &TransportConfig) -> String {
    match t {
        TransportConfig::Rtu { path, .. } => format!(
            "rtu:{}",
            if cfg!(windows) {
                path.trim_start_matches("\\\\.\\").to_uppercase()
            } else {
                path.clone()
            }
        ),
        TransportConfig::Tcp { host, port, .. }
        | TransportConfig::RtuOverTcp { host, port, .. } => {
            format!("tcp:{}:{port}", host.to_lowercase())
        }
    }
}
fn read_request(args: &Value) -> Result<ModbusRequest, String> {
    let function = number(args, "function", 3, 1, 4)?;
    let addr = number(args, "address", 0, 0, 65535)? as u16;
    let qty = number(args, "quantity", 1, 1, 125)? as u16;
    if u32::from(addr) + u32::from(qty) > 65536 {
        return Err("address range overflows".into());
    }
    Ok(match function {
        1 => ModbusRequest::ReadCoils { addr, qty },
        2 => ModbusRequest::ReadDiscreteInputs { addr, qty },
        3 => ModbusRequest::ReadHoldingRegisters { addr, qty },
        _ => ModbusRequest::ReadInputRegisters { addr, qty },
    })
}
fn response(r: &ModbusResponse) -> Value {
    match r {
        ModbusResponse::Registers(v) => json!({"registers":v}),
        ModbusResponse::Bits(v) => json!({"bits":v}),
        ModbusResponse::Raw(v) => json!({"bytes":v.to_vec()}),
        ModbusResponse::WriteAck => json!({"acknowledged":true}),
    }
}
fn config_for(state: &super::State, path: &std::path::Path) -> Result<ResolvedConfig, String> {
    validate(state.active.as_ref().unwrap_or(&state.desired), path)
}
fn channel<'a>(
    cfg: &'a ResolvedConfig,
    id: &str,
) -> Result<&'a gateway_config::ResolvedChannel, String> {
    cfg.channels
        .iter()
        .find(|c| c.name == id)
        .ok_or_else(|| format!("unknown channel {id}"))
}
fn access(owner: &Controller, state: &super::State, args: &Value) -> Result<Vec<Access>, String> {
    let cfg = config_for(state, &owner.path)?;
    if let Some(id) = args.get("channel_id").and_then(Value::as_str) {
        if args.get("transports").is_some() {
            return Err("choose channel_id or transports, not both".into());
        }
        let ch = channel(&cfg, id)?;
        if let Some(stack) = &state.stack {
            let sender = stack
                .writer(ch.id)
                .ok_or("channel is disabled or not running")?;
            return Ok(vec![Access::Queued {
                sender,
                transport: ch.transport.clone(),
            }]);
        }
        return Ok(vec![Access::Offline(ch.transport.clone())]);
    }
    let targets = args
        .get("transports")
        .and_then(Value::as_array)
        .ok_or("channel_id or transports required")?;
    if targets.is_empty() || targets.len() > 256 {
        return Err("transports must contain 1..256 targets".into());
    }
    let mut result = Vec::new();
    for target in targets {
        let t: TransportConfig =
            serde_json::from_value(target.clone()).map_err(|e| e.to_string())?;
        let key = resource(&t);
        if let Some(stack) = &state.stack {
            if let Some(ch) = cfg
                .channels
                .iter()
                .find(|c| c.enabled && resource(&c.transport) == key)
            {
                // Prevent a scan with different serial settings from silently
                // using another baud/parity than the requested candidate.
                if serde_json::to_value(&ch.transport).unwrap() != serde_json::to_value(&t).unwrap()
                {
                    return Err("transport is owned by a live channel with different settings; use channel_id".into());
                }
                result.push(Access::Queued {
                    sender: stack.writer(ch.id).ok_or("channel unavailable")?,
                    transport: t,
                });
                continue;
            }
            if matches!(t, TransportConfig::RtuOverTcp { .. }) {
                return Err("for RTU-over-TCP while polling use a configured channel_id (host aliases can share a physical bus)".into());
            }
        }
        match &t {
            TransportConfig::Rtu {
                baud,
                data_bits,
                stop_bits,
                path,
                ..
            } if *baud == 0
                || !(5..=8).contains(data_bits)
                || !(1..=2).contains(stop_bits)
                || path.is_empty() =>
            {
                return Err("invalid serial settings".into())
            }
            TransportConfig::Tcp { host, port, .. }
            | TransportConfig::RtuOverTcp { host, port, .. }
                if host.is_empty() || *port == 0 =>
            {
                return Err("invalid TCP endpoint".into())
            }
            _ => {}
        }
        result.push(Access::Offline(t));
    }
    Ok(result)
}
fn bound_connect(mut t: TransportConfig) -> TransportConfig {
    match &mut t {
        TransportConfig::Tcp {
            connect_timeout_ms, ..
        }
        | TransportConfig::RtuOverTcp {
            connect_timeout_ms, ..
        } => *connect_timeout_ms = (*connect_timeout_ms).clamp(50, 2000),
        _ => {}
    };
    t
}
async fn queued(
    sender: &tokio::sync::mpsc::Sender<mb_poller::WriteCommand>,
    unit: u8,
    req: ModbusRequest,
    timeout: Duration,
    device: Option<mb_types::DeviceId>,
) -> Result<ModbusResponse, ProtoError> {
    let deadline = Instant::now() + timeout + Duration::from_secs(5);
    let (reply, rx) = tokio::sync::oneshot::channel();
    let diagnostic = device
        .is_none()
        .then_some(mb_poller::DiagnosticTarget { unit, timeout });
    let cmd = mb_poller::WriteCommand {
        diagnostic,
        device: device.unwrap_or(mb_types::DeviceId(u32::MAX)),
        req,
        reply,
        deadline,
    };
    let until = tokio::time::Instant::from_std(deadline);
    tokio::time::timeout_at(until, async {
        sender
            .send(cmd)
            .await
            .map_err(|_| ProtoError::NotConnected)?;
        rx.await.map_err(|_| ProtoError::NotConnected)?
    })
    .await
    .map_err(|_| ProtoError::Timeout)?
}
pub async fn start(owner: &Arc<Controller>, args: Value) -> Result<Value, String> {
    let from = number(&args, "unit_start", 1, 1, 247)? as u8;
    let to = number(&args, "unit_end", 16, 1, 247)? as u8;
    if to < from {
        return Err("unit_end must be >= unit_start".into());
    }
    let timeout = Duration::from_millis(number(&args, "timeout_ms", 300, 50, 2000)?);
    let delay = Duration::from_millis(number(&args, "delay_ms", 20, 10, 1000)?);
    let req = read_request(&args)?;
    let state = owner.state.lock().await;
    if owner.is_closed() {
        return Err("controller is shutting down".into());
    }
    let targets = access(owner, &state, &args)?;
    let resources: HashSet<_> = targets
        .iter()
        .map(|a| match a {
            Access::Queued { transport, .. } | Access::Offline(transport) => resource(transport),
        })
        .collect();
    let offline = targets.iter().any(|a| matches!(a, Access::Offline(_)));
    let total = targets.len() * usize::from(to - from + 1);
    let id = random_id()?;
    let job = Arc::new(ScanJob {
        offline,
        cancel: AtomicBool::new(false),
        task: parking_lot::Mutex::new(None),
        resources,
        data: parking_lot::Mutex::new(
            json!({"job_id":id,"created_ms":now_ms(),"status":"running","total_requests":total,"completed_requests":0,"found_count":0,"results":[],"target_errors":[],"read_errors":0,"truncated":false}),
        ),
    });
    {
        let mut jobs = owner.jobs.lock();
        if jobs.values().filter(|j| j.running()).count() >= 4 {
            return Err("four scans are already running".into());
        }
        if jobs
            .values()
            .any(|j| j.running() && !j.resources.is_disjoint(&job.resources))
        {
            return Err("a scan already owns this transport; finish or cancel it first".into());
        }
        if jobs.len() >= 32 {
            let done = jobs
                .iter()
                .find(|(_, j)| !j.running())
                .map(|(id, _)| id.clone());
            if let Some(id) = done {
                jobs.remove(&id);
            } else {
                return Err("scan history full".into());
            }
        }
        jobs.insert(id.clone(), Arc::clone(&job));
    }
    let worker = Arc::clone(&job);
    let task = tokio::spawn(async move {
        let mut interrupted = false;
        'targets: for target in targets {
            if worker.cancel.load(Ordering::Acquire) {
                break;
            }
            let (transport_cfg, sender) = match &target {
                Access::Queued { sender, transport } => (transport.clone(), Some(sender.clone())),
                Access::Offline(t) => (t.clone(), None),
            };
            let mut connection = None;
            if sender.is_none() {
                match Transport::connect(&bound_connect(transport_cfg.clone())).await {
                    Ok(c) => connection = Some(c),
                    Err(e) => {
                        worker.data.lock()["target_errors"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"transport":transport_cfg,"error":e.to_string()}));
                        continue;
                    }
                }
            }
            for unit in from..=to {
                if worker.cancel.load(Ordering::Acquire) {
                    break;
                }
                // A timed-out RTU frame may arrive late. Reconnect before the
                // next unit so it cannot be attributed to the wrong responder.
                if sender.is_none() && connection.is_none() {
                    match Transport::connect(&bound_connect(transport_cfg.clone())).await {
                        Ok(c) => connection = Some(c),
                        Err(e) => {
                            worker.data.lock()["target_errors"]
                                .as_array_mut()
                                .unwrap()
                                .push(json!({"transport":transport_cfg,"error":e.to_string()}));
                            break;
                        }
                    }
                }
                let result = if let Some(sender) = &sender {
                    queued(sender, unit, req.clone(), timeout, None).await
                } else {
                    connection
                        .as_mut()
                        .unwrap()
                        .request(unit, &req, timeout)
                        .await
                };
                if matches!(result, Err(ProtoError::NotConnected)) {
                    interrupted = true;
                    break 'targets;
                }
                let found = match &result {
                    Ok(reply) => Some(
                        json!({"unit_id":unit,"transport":transport_cfg,"reply":response(reply),"identity":"Modbus responder; model/vendor not identified"}),
                    ),
                    Err(ProtoError::Exception(code))
                        if !matches!(
                            code,
                            mb_proto::ExceptionCode::GatewayPathUnavailable
                                | mb_proto::ExceptionCode::GatewayTargetDevice
                        ) =>
                    {
                        Some(
                            json!({"unit_id":unit,"transport":transport_cfg,"exception":format!("{code:?}"),"identity":"Modbus responder; register unsupported, model/vendor not identified"}),
                        )
                    }
                    _ => None,
                };
                let reconnect = connection.as_ref().is_some_and(|c| {
                    result.as_ref().is_err_and(|e| {
                        e.is_fatal() || (c.is_half_duplex() && matches!(e, ProtoError::Timeout))
                    })
                });
                {
                    let mut d = worker.data.lock();
                    let n = d["completed_requests"].as_u64().unwrap() + 1;
                    d["completed_requests"] = json!(n);
                    if let Some(row) = found {
                        let n = d["found_count"].as_u64().unwrap() + 1;
                        d["found_count"] = json!(n);
                        if d["results"].as_array().unwrap().len() < 4096 {
                            d["results"].as_array_mut().unwrap().push(row);
                        } else {
                            d["truncated"] = json!(true);
                        }
                    } else {
                        let n = d["read_errors"].as_u64().unwrap() + 1;
                        d["read_errors"] = json!(n);
                    }
                }
                if reconnect {
                    if let Some(c) = connection.take() {
                        let _ = c.disconnect().await;
                    }
                }
                tokio::time::sleep(delay).await;
            }
            if let Some(c) = connection.take() {
                let _ = c.disconnect().await;
            }
        }
        let status = if worker.cancel.load(Ordering::Acquire) {
            "cancelled"
        } else if interrupted {
            "interrupted (runtime reconfigured/stopped)"
        } else {
            "completed"
        };
        let mut d = worker.data.lock();
        d["status"] = json!(status);
        d["finished_ms"] = json!(now_ms());
    });
    *job.task.lock() = Some(task);
    drop(state);
    Ok(json!({"job_id":id,"status":"running","total_requests":total,"writes_performed":false}))
}
pub fn status(owner: &Controller, name: &str, args: &Value) -> Result<Value, String> {
    let id = string(args, "job_id")?;
    let job = owner
        .jobs
        .lock()
        .get(id)
        .cloned()
        .ok_or("unknown scan job")?;
    if name == "cancel_scan" {
        job.cancel.store(true, Ordering::Release);
        return Ok(
            json!({"job_id":id,"cancel_requested":true,"status":job.data.lock()["status"],"note":"current transaction finishes before connection release"}),
        );
    }
    let offset = number(args, "offset", 0, 0, usize::MAX as u64)? as usize;
    let limit = number(args, "limit", 100, 1, 500)? as usize;
    let mut result = job.data.lock().clone();
    let rows = result["results"].as_array().unwrap();
    let total = rows.len();
    result["results"] = json!(rows
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>());
    result["stored_results"] = json!(total);
    result["offset"] = json!(offset);
    Ok(result)
}
pub async fn request(owner: &Arc<Controller>, name: &str, args: &Value) -> Result<Value, String> {
    // Hold the controller lock through a one-shot transaction: configuration
    // edits cannot change its target or open the same serial port meanwhile.
    let state = owner.state.lock().await;
    if owner.is_closed() {
        return Err("controller is shutting down".into());
    }
    let cfg = config_for(&state, &owner.path)?;
    let ch = channel(&cfg, string(args, "channel_id")?)?;
    let timeout = Duration::from_millis(number(args, "timeout_ms", 500, 50, 2000)?);
    let (unit, req, device) = if name == "read_modbus" {
        (
            number(args, "unit_id", 1, 1, 247)? as u8,
            read_request(args)?,
            None,
        )
    } else {
        if args["confirm"] != true {
            return Err("explicit human confirmation required (confirm=true)".into());
        }
        let revision = match &state.applied {
            Some(revision) => revision.clone(),
            None => fingerprint(&read_file(&owner.path)?),
        };
        if string(args, "expected_revision")? != revision {
            return Err("configuration changed; re-read the current revision".into());
        }
        let dev = ch
            .devices
            .iter()
            .find(|d| d.name == string(args, "device_id").unwrap_or(""))
            .ok_or("unknown configured device")?;
        if !dev.enabled || !ch.enabled || dev.unit_id == 0 {
            return Err("disabled devices and broadcast writes are rejected".into());
        }
        let addr = number(args, "address", 0, 0, 65535)? as u16;
        let function = number(args, "function", 6, 5, 16)?;
        let words = || -> Result<Vec<u16>, String> {
            let values = args["values"].as_array().ok_or("values array required")?;
            if values.is_empty() || values.len() > 123 {
                return Err("values must contain 1..123 registers".into());
            }
            values
                .iter()
                .map(|v| {
                    v.as_u64()
                        .filter(|v| *v <= 65535)
                        .map(|v| v as u16)
                        .ok_or_else(|| "register values must be integers 0..65535".into())
                })
                .collect()
        };
        let req = match function {
            5 => ModbusRequest::WriteSingleCoil {
                addr,
                value: args["value"]
                    .as_bool()
                    .ok_or("coil value must be boolean")?,
            },
            6 => ModbusRequest::WriteSingleRegister {
                addr,
                value: number(args, "value", u64::MAX, 0, 65535)? as u16,
            },
            15 => {
                let v = args["values"].as_array().ok_or("values required")?;
                if v.is_empty() || v.len() > 123 {
                    return Err("1..123 coil values required".into());
                }
                ModbusRequest::WriteMultipleCoils {
                    addr,
                    values: v
                        .iter()
                        .map(|v| v.as_bool().ok_or("coil values must be booleans"))
                        .collect::<Result<Vec<_>, _>>()?,
                }
            }
            16 => ModbusRequest::WriteMultipleRegisters {
                addr,
                values: words()?,
            },
            _ => return Err("write function must be 5, 6, 15 or 16".into()),
        };
        let count = match &req {
            ModbusRequest::WriteMultipleCoils { values, .. } => values.len(),
            ModbusRequest::WriteMultipleRegisters { values, .. } => values.len(),
            _ => 1,
        };
        if usize::from(addr) + count > 65536 {
            return Err("write address range overflows".into());
        }
        (dev.unit_id, req, Some(dev.id))
    };
    let result = if let Some(stack) = &state.stack {
        let sender = stack.writer(ch.id).ok_or("channel not running")?;
        queued(&sender, unit, req, timeout, device).await
    } else {
        if owner
            .jobs
            .lock()
            .values()
            .any(|j| j.running() && j.resources.contains(&resource(&ch.transport)))
        {
            return Err("an offline scan owns this transport".into());
        }
        let mut transport = Transport::connect(&bound_connect(ch.transport.clone()))
            .await
            .map_err(|e| e.to_string())?;
        let result = transport.request(unit, &req, timeout).await;
        let _ = transport.disconnect().await;
        result
    };
    result.map(|r|json!({"unit_id":unit,"channel_id":ch.name,"response":response(&r),"acknowledged":matches!(r,ModbusResponse::WriteAck)})).map_err(|e|e.to_string())
}
