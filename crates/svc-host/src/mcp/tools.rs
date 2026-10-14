use super::{
    fingerprint, parse_file, read_file, redacted, snapshot_json, validate, Controller,
    MAX_CONFIG_BYTES,
};
use serde_json::{json, Value};
use std::{
    io::{Read, Seek, SeekFrom, Write},
    sync::{atomic::Ordering, Arc},
};
use tags_core::TypedReader;

pub fn examples() -> Value {
    json!({
        "channel":{"id":"plc","transport":{"type":"tcp","host":"192.168.1.10","port":502},"devices":[]},
        "device":{"id":"meter","unit_id":1,"registers":[]},
        "tag":{"tag":"meter.temperature","address":0,"poll_group":"fast","function":"read_holding_registers","data_type":"i16","formula":"raw * 0.1","units":"degC"},
        "poll_group":{"id":"fast","period_ms":500},
        "formula_examples":["raw * 0.1", "tag(\"phase.a\") + tag(\"phase.b\")", "if(raw > 100, 1, 0)"],
        "workflow":["get_config: revision is the saved file SHA256", "Edit with expected_revision; use dry_run first if needed", "apply=false stages a valid configuration without touching the running stack", "Startup settings: server_stop(confirm=true), edit, server_start", "For device writes use the applied_revision from server_status, confirm only after explicit human approval", "start_scan returns a job_id; scan_status and cancel_scan do not interrupt polling", "read_tags includes calculated values before publication deadband"]
    })
}
fn prop(t: &str, description: &str) -> Value {
    json!({"type":t,"description":description})
}
fn tool(
    name: &str,
    description: &str,
    props: Value,
    required: &[&str],
    read: bool,
    destructive: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":props,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read,"destructiveHint":destructive,"idempotentHint":read,"openWorldHint":false}})
}
pub fn definitions() -> Vec<Value> {
    let text = prop("string", "Identifier");
    let confirm = json!({"type":"boolean","const":true,"description":"Explicit human approval of this exact operation is required"});
    let mut out=vec![
        tool("server_status","Live runtime, applied configuration revision, endpoint and channel counters",json!({}),&[],true,false),
        tool("connection_info","Actual executable/config/storage paths and ready-to-copy Codex TOML, Claude JSON and stdio command for this instance",json!({}),&[],true,false),
        tool("check_access","Check configuration write access, atomic replacement of scratch files and tag-storage access. Creates temporary probe files and may open/create storage; never accesses devices or starts polling.",json!({}),&[],false,false),
        tool("get_config","Read saved and active configuration; passwords are redacted. Use revision for edits and applied_revision for live writes.",json!({}),&[],true,false),
        tool("validate_config","Validate JSON schema, sources, read/write formulas, unknown dependencies and cycles",json!({"config":{"type":"object"}}),&["config"],true,false),
        tool("list_devices","List configured channels and devices; does not guess model/vendor or prove a hardware response",json!({}),&[],true,false),
        tool("read_tags","Read published values and latest calculated values before deadband; pagination and name filter",json!({"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":1000},"filter":{"type":"string"}}),&[],true,false),
        tool("get_tag","Read a named tag's configuration, published and calculated value",json!({"tag":text}),&["tag"],true,false),
        tool("test_formula","Evaluate an expression using raw/value and supplied numeric tags or live calculated values; no Modbus access",json!({"formula":text,"input":{"type":"number"},"variable":{"type":"string","enum":["raw","value"]},"tags":{"type":"object","additionalProperties":{"type":"number"}}}),&["formula","input"],true,false),
        tool("get_history","Read the persisted history of a named tag, newest entries last",json!({"tag":text,"limit":{"type":"integer","minimum":1,"maximum":1000}}),&["tag"],true,false),
        tool("get_logs","Read up to 200 recent lines from this instance's configured log directory",json!({"limit":{"type":"integer","minimum":1,"maximum":200}}),&[],true,false),
        tool("get_audit","Read recent MCP operation names, duration and outcome; arguments and secrets are never logged",json!({}),&[],true,false),
        tool("list_serial_ports","Enumerate local COM/tty ports without opening them",json!({}),&[],true,false),
        tool("server_start","Start polling and OPC UA using the saved configuration; repeated calls do not create another runtime",json!({}),&[],false,false),
        tool("server_stop","ИЗМЕНЯЕТ ОБЪЕКТ: stops this instance's polling/OPC UA. Ask the human before setting confirm=true. MCP stays available.",json!({"confirm":confirm}),&["confirm"],false,true),
        tool("server_restart","ИЗМЕНЯЕТ ОБЪЕКТ: stops and restarts polling, disconnecting OPC UA clients. Requires explicit human approval.",json!({"confirm":confirm}),&["confirm"],false,true),
        tool("apply_config","Apply the saved configuration to the running runtime without restarting it",json!({"expected_revision":text}),&["expected_revision"],false,false),
        tool("shutdown_controller","ИЗМЕНЯЕТ ОБЪЕКТ: stops polling and closes this MCP controller. Requires explicit human approval.",json!({"confirm":confirm}),&["confirm"],false,true),
        tool("write_tag","ИЗМЕНЯЕТ ОБЪЕКТ: write engineering value to a writable tag using the active inverse formula. Ask the human for this exact tag/value first. Good is returned only after device acknowledgement.",json!({"tag":text,"value":{"type":["number","boolean"]},"expected_revision":text,"confirm":confirm}),&["tag","value","expected_revision","confirm"],false,true),
        tool("read_modbus","Read FC01/02/03/04 on a configured channel. Running channels use their existing queue; no second COM connection.",json!({"channel_id":text,"unit_id":{"type":"integer","minimum":1,"maximum":247},"function":{"type":"integer","enum":[1,2,3,4]},"address":{"type":"integer","minimum":0,"maximum":65535},"quantity":{"type":"integer","minimum":1,"maximum":125},"timeout_ms":{"type":"integer","minimum":50,"maximum":2000}}),&["channel_id","unit_id","function","address","quantity"],true,false),
        tool("write_modbus","ИЗМЕНЯЕТ ОБЪЕКТ: raw FC05/06/15/16 write to a named configured device. Requires approval of the exact address/value, confirm=true and the current applied (or saved when stopped) revision. Broadcasts are rejected.",json!({"channel_id":text,"device_id":text,"function":{"type":"integer","enum":[5,6,15,16]},"address":{"type":"integer","minimum":0,"maximum":65535},"value":{"type":["integer","boolean"]},"values":{"type":"array","minItems":1,"maxItems":123,"items":{"type":["integer","boolean"]}},"expected_revision":text,"confirm":confirm}),&["channel_id","device_id","function","address","expected_revision","confirm"],false,true),
        tool("start_scan","Background discovery by standard Modbus reads. Supply channel_id to share an active bus, or explicit transports (TCP, RTU, RTU-over-TCP). No automatic device creation or guessed identity. Poll scan_status; cancel_scan stops between complete requests.",json!({"channel_id":text,"transports":{"type":"array","minItems":1,"maxItems":256,"items":{"type":"object"}},"unit_start":{"type":"integer","minimum":1,"maximum":247},"unit_end":{"type":"integer","minimum":1,"maximum":247},"function":{"type":"integer","enum":[1,2,3,4]},"address":{"type":"integer","minimum":0,"maximum":65535},"quantity":{"type":"integer","minimum":1,"maximum":125},"timeout_ms":{"type":"integer","minimum":50,"maximum":2000},"delay_ms":{"type":"integer","minimum":10,"maximum":1000}}),&[],false,false),
        tool("scan_status","Read a background scan's progress and verified responders, with paginated results",json!({"job_id":text,"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":500}}),&["job_id"],true,false),
        tool("cancel_scan","Request cancellation; the current transaction finishes before releasing a serial connection",json!({"job_id":text}),&["job_id"],false,false),
    ];
    for (name,desc,extra,req) in [
        ("set_config","Replace the complete configuration",json!({"config":{"type":"object"}}),vec!["config"]),
        ("patch_config","Apply a JSON Merge Patch (arrays replace); preserve existing secrets by editing only needed fields",json!({"patch":{"type":"object"}}),vec!["patch"]),
        ("upsert_channel","Add/replace a complete channel object (id, transport, devices)",json!({"channel":{"type":"object"}}),vec!["channel"]),
        ("delete_channel","Delete a named channel and its devices/tags",json!({"channel_id":text}),vec!["channel_id"]),
        ("upsert_device","Add/replace a complete device object (id, unit_id, registers) in a named channel",json!({"channel_id":text,"device":{"type":"object"}}),vec!["channel_id","device"]),
        ("delete_device","Delete a device and its tags",json!({"channel_id":text,"device_id":text}),vec!["channel_id","device_id"]),
        ("upsert_tag","Add/replace a complete register/tag object in a named device",json!({"channel_id":text,"device_id":text,"tag":{"type":"object"}}),vec!["channel_id","device_id","tag"]),
        ("delete_tag","Delete a named tag; referenced tags cannot be removed until their formulas are updated",json!({"tag":text}),vec!["tag"]),
        ("set_formula","Set or remove a named tag's formula and/or write_formula",json!({"tag":text,"formula":{"type":["string","null"]},"write_formula":{"type":["string","null"]}}),vec!["tag"]),
        ("upsert_poll_group","Add/replace a poll group (id, period_ms, priority)",json!({"poll_group":{"type":"object"}}),vec!["poll_group"]),
    ] {
        let mut props=extra; props["expected_revision"]=prop("string","SHA256 revision from get_config; stale edits are rejected");
        props["apply"]=json!({"type":"boolean","default":true,"description":"false stages the file without changing the running runtime"});
        props["dry_run"]=json!({"type":"boolean","default":false});
        let mut required=req; required.push("expected_revision");
        out.push(tool(name,&format!("{desc}. Validate first, atomic save with one .bak backup; hot apply by default."),props,&required,false,name.starts_with("delete")));
    }
    out
}
pub fn known(name: &str) -> bool {
    definitions().iter().any(|v| v["name"] == name)
}
pub(super) fn string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be a string"))
}
pub(super) fn number(
    args: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    let value = match args.get(key) {
        None => default,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("{key} must be an unsigned integer"))?,
    };
    if value < min || value > max {
        Err(format!("{key} outside {min}..={max}"))
    } else {
        Ok(value)
    }
}
fn approved(args: &Value) -> Result<(), String> {
    if args["confirm"] == true {
        Ok(())
    } else {
        Err("explicit human confirmation required (confirm=true)".into())
    }
}
fn merge(target: &mut Value, patch: &Value) {
    if let Value::Object(changes) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        for (key, value) in changes {
            if value.is_null() {
                target.as_object_mut().unwrap().remove(key);
            } else {
                merge(
                    target
                        .as_object_mut()
                        .unwrap()
                        .entry(key.clone())
                        .or_insert(Value::Null),
                    value,
                );
            }
        }
    } else {
        *target = patch.clone();
    }
}
fn array<'a>(v: &'a mut Value, key: &str) -> Result<&'a mut Vec<Value>, String> {
    v.get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("{key} array missing"))
}
fn find<'a>(items: &'a mut [Value], key: &str, name: &str) -> Result<&'a mut Value, String> {
    items
        .iter_mut()
        .find(|v| v[key] == name)
        .ok_or_else(|| format!("unknown {key}: {name}"))
}
fn upsert(items: &mut Vec<Value>, item: Value, key: &str) -> Result<(), String> {
    let name = item
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} required"))?
        .to_owned();
    if let Some(old) = items.iter_mut().find(|v| v[key] == name) {
        *old = item;
    } else {
        items.push(item);
    }
    Ok(())
}
fn edit(v: &mut Value, name: &str, args: &Value) -> Result<(), String> {
    match name {
        "set_config" => {
            *v = args
                .get("config")
                .filter(|v| v.is_object())
                .ok_or("config required")?
                .clone()
        }
        "patch_config" => merge(
            v,
            args.get("patch")
                .filter(|v| v.is_object())
                .ok_or("patch required")?,
        ),
        "upsert_channel" => upsert(
            array(v, "channels")?,
            args.get("channel").ok_or("channel required")?.clone(),
            "id",
        )?,
        "delete_channel" => {
            let id = string(args, "channel_id")?;
            let items = array(v, "channels")?;
            find(items, "id", id)?;
            items.retain(|v| v["id"] != id);
        }
        "upsert_device" | "delete_device" | "upsert_tag" => {
            let ch = find(array(v, "channels")?, "id", string(args, "channel_id")?)?;
            let devices = array(ch, "devices")?;
            match name {
                "upsert_device" => upsert(
                    devices,
                    args.get("device").ok_or("device required")?.clone(),
                    "id",
                )?,
                "delete_device" => {
                    let id = string(args, "device_id")?;
                    find(devices, "id", id)?;
                    devices.retain(|v| v["id"] != id);
                }
                _ => {
                    let dev = find(devices, "id", string(args, "device_id")?)?;
                    upsert(
                        array(dev, "registers")?,
                        args.get("tag").ok_or("tag required")?.clone(),
                        "tag",
                    )?;
                }
            }
        }
        "delete_tag" | "set_formula" => {
            let tag = string(args, "tag")?;
            let mut found = false;
            for ch in array(v, "channels")? {
                for dev in array(ch, "devices")? {
                    let regs = array(dev, "registers")?;
                    if name == "delete_tag" {
                        if regs.iter().any(|r| r["tag"] == tag) {
                            regs.retain(|r| r["tag"] != tag);
                            found = true;
                        }
                    } else if let Some(reg) = regs.iter_mut().find(|r| r["tag"] == tag) {
                        for key in ["formula", "write_formula"] {
                            if let Some(value) = args.get(key) {
                                if !value.is_null() && !value.is_string() {
                                    return Err(format!("{key} must be string or null"));
                                }
                                reg[key] = value.clone();
                                found = true;
                            }
                        }
                    }
                }
            }
            if !found {
                return Err("tag unknown or no formula fields supplied".into());
            }
        }
        "upsert_poll_group" => upsert(
            array(v, "poll_groups")?,
            args.get("poll_group").ok_or("poll_group required")?.clone(),
            "id",
        )?,
        _ => return Err("unknown configuration edit".into()),
    }
    if v.pointer("/opcua/users")
        .and_then(Value::as_array)
        .is_some_and(|users| {
            users.iter().any(|u| {
                ["password", "password_hash"]
                    .iter()
                    .any(|k| u[*k] == "<redacted>")
            })
        })
    {
        return Err("cannot save redacted passwords; use a targeted patch/upsert instead".into());
    }
    Ok(())
}
fn prepared(path: &std::path::Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().unwrap()).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    Ok(file)
}
async fn mutate(owner: &Arc<Controller>, name: &str, args: &Value) -> Result<Value, String> {
    let mut state = owner.state.lock().await;
    if owner.is_closed() {
        return Err("controller is shutting down".into());
    }
    let original = read_file(&owner.path)?;
    let previous = fingerprint(&original);
    if string(args, "expected_revision")? != previous {
        return Err("configuration changed; re-read get_config before editing".into());
    }
    let mut candidate = if name == "set_config" {
        Value::Null
    } else {
        parse_file(&original)?
    };
    edit(&mut candidate, name, args)?;
    let cfg = validate(&candidate, &owner.path)?;
    let apply = args
        .get("apply")
        .map(|v| v.as_bool().ok_or("apply must be boolean"))
        .transpose()?
        .unwrap_or(true);
    if apply && owner.jobs.lock().values().any(|j| j.offline && j.running()) {
        return Err("finish or cancel offline scans before changing the active runtime".into());
    }
    if apply && cfg.mcp != owner.settings {
        return Err("MCP host/port/enabled changes require staging (apply=false) and restarting the controller".into());
    }
    if apply {
        if let Some(stack) = &state.stack {
            stack.can_reload(&cfg)?;
        }
    }
    if args["dry_run"] == true {
        return Ok(
            json!({"valid":true,"saved":false,"config":redacted(candidate),"would_apply":apply&&state.stack.is_some(),"revision":previous}),
        );
    }
    let bytes = serde_json::to_vec_pretty(&candidate).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err("configuration exceeds 8 MiB".into());
    }
    let ready = prepared(&owner.path, &bytes)?;
    let backup = prepared(&owner.path, &original)?;
    if read_file(&owner.path)? != original {
        return Err("file changed during validation; re-read before editing".into());
    }
    let mut backup_path = owner.path.as_os_str().to_os_string();
    backup_path.push(".bak");
    backup
        .persist(std::path::PathBuf::from(backup_path))
        .map_err(|e| {
            format!(
                "cannot replace configuration backup: {e}; check Modify access to {}",
                owner.path.parent().unwrap().display()
            )
        })?;
    ready.persist(&owner.path).map_err(|e| format!("cannot replace configuration {}: {e}; check Modify access to the file and its directory",owner.path.display()))?;
    let revision = fingerprint(&bytes);
    state.seen = bytes;
    state.desired = candidate.clone();
    let mut applied = false;
    if apply {
        if let Some(stack) = state.stack.as_mut() {
            match stack.reload(cfg).await {
                Ok(()) => {
                    state.applied = Some(revision.clone());
                    state.active = Some(candidate);
                    state.last_error = None;
                    applied = true;
                }
                Err(e) => {
                    state.last_error = Some(e.clone());
                    return Ok(json!({"saved":true,"applied":false,"revision":revision,"error":e}));
                }
            }
        }
    }
    Ok(
        json!({"saved":true,"applied":applied,"revision":revision,"applied_revision":state.applied,"staged":!apply&&state.stack.is_some()}),
    )
}
pub async fn call(owner: &Arc<Controller>, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "connection_info" => return super::connection_info(&owner.path),
        "check_access" => {
            let state = owner.state.lock().await;
            return super::check_access(
                &owner.path,
                state
                    .stack
                    .as_ref()
                    .map(|s| s.config().gateway.data_dir.as_str()),
            );
        }
        "server_start" => {
            owner.start_server().await?;
            return Ok(json!({"running":true}));
        }
        "server_stop" => {
            approved(&args)?;
            owner.stop_server().await;
            return Ok(json!({"running":false,"mcp_available":true}));
        }
        "server_restart" => {
            approved(&args)?;
            let cfg = validate(&parse_file(&read_file(&owner.path)?)?, &owner.path)?;
            if cfg.mcp != owner.settings {
                return Err("restart the controller to apply staged MCP settings".into());
            }
            if owner.jobs.lock().values().any(|j| j.offline && j.running()) {
                return Err("finish or cancel offline scans before restarting".into());
            }
            owner.stop_server().await;
            owner.start_server().await?;
            return Ok(json!({"running":true}));
        }
        "apply_config" => {
            if fingerprint(&read_file(&owner.path)?) != string(&args, "expected_revision")? {
                return Err("configuration changed".into());
            }
            owner.reload_file().await?;
            return Ok(json!({"applied":owner.view().running}));
        }
        "shutdown_controller" => {
            approved(&args)?;
            super::transport::mark_stop(owner)?;
            owner.closed.store(true, Ordering::Release);
            owner.stop_server().await;
            let controller = Arc::clone(owner);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                controller.shutdown().await;
            });
            return Ok(json!({"shutting_down":true}));
        }
        "start_scan" => return super::discovery::start(owner, args).await,
        "scan_status" | "cancel_scan" => return super::discovery::status(owner, name, &args),
        "read_modbus" | "write_modbus" => {
            return super::discovery::request(owner, name, &args).await
        }
        "list_serial_ports" => {
            return Ok(json!({"ports":mb_proto::available_serial_ports(),"opened":false}))
        }
        "set_config" | "patch_config" | "upsert_channel" | "delete_channel" | "upsert_device"
        | "delete_device" | "upsert_tag" | "delete_tag" | "set_formula" | "upsert_poll_group" => {
            return mutate(owner, name, &args).await
        }
        _ => {}
    }
    let state = owner.state.lock().await;
    let value = state.active.as_ref().unwrap_or(&state.desired);
    match name {
        "server_status" => {
            let endpoint = owner.endpoint.read();
            let metrics:Vec<_>=state.stack.as_ref().map(|s|s.channel_metrics().into_iter().map(|(name,m)|{let connected=m.connected.load(Ordering::Relaxed);let error=m.last_error.read().clone();let m=m.snapshot();json!({"channel":name,"connected":connected,"last_error":error,"requests_ok":m.reqs_ok,"requests_error":m.reqs_err,"timeouts":m.timeouts,"exceptions":m.exceptions,"protocol_errors":m.protocol_errors,"reconnects":m.reconnects,"writes_ok":m.writes_ok,"writes_error":m.writes_err})}).collect()).unwrap_or_default();
            Ok(
                json!({"attached":true,"config_id":super::config_id(&owner.path),"instance_name":state.desired.pointer("/gateway/instance_name").and_then(Value::as_str).unwrap_or("OPC Modbus"),"running":state.stack.is_some(),"config_path":owner.path,"revision":read_file(&owner.path).ok().map(|b|fingerprint(&b)),"applied_revision":state.applied,
                "runtime_revision":state.stack.as_ref().map(|s|s.revision()),"uptime_seconds":state.stack.as_ref().map(|s|s.uptime_seconds()),"opcua_endpoint":state.stack.as_ref().and_then(|s|s.endpoint_url()),
                "opcua_sessions":state.stack.as_ref().and_then(|s|s.opcua()).map(|o|o.session_count()),"mcp_url":endpoint.as_ref().map(|e|&e.url),"instance_id":endpoint.as_ref().map(|e|&e.instance),
                "mcp_clients":owner.client_views(),"last_error":state.last_error,"channels":metrics,"build":crate::build_info()}),
            )
        }
        "get_config" => {
            let bytes = read_file(&owner.path)?;
            let parsed = parse_file(&bytes);
            Ok(
                json!({"revision":fingerprint(&bytes),"config":parsed.as_ref().ok().cloned().map(redacted),"parse_error":parsed.err(),"active_config":state.active.clone().map(redacted),"applied_revision":state.applied}),
            )
        }
        "validate_config" => {
            let cfg = validate(args.get("config").ok_or("config required")?, &owner.path)?;
            Ok(
                json!({"valid":true,"tags":cfg.tag_count(),"warnings":cfg.warnings.iter().map(ToString::to_string).collect::<Vec<_>>()}),
            )
        }
        "get_audit" => Ok(json!({"operations":state.audit})),
        "list_devices" => {
            let channels = value.get("channels").cloned().unwrap_or(json!([]));
            Ok(
                json!({"configuration":if state.stack.is_some(){"active"}else{"saved"},"channels":channels,"hardware_identity":"configured names only; use start_scan to verify responders"}),
            )
        }
        "read_tags" => {
            let offset = number(&args, "offset", 0, 0, usize::MAX as u64)? as usize;
            let limit = number(&args, "limit", 100, 1, 1000)? as usize;
            let filter = args["filter"].as_str().unwrap_or("").to_lowercase();
            let mut rows = Vec::new();
            if let Some(stack) = &state.stack {
                let store = stack.typed();
                let calculated = stack.calculated();
                for (id, snap) in store.snapshot_all() {
                    let tag = store.name(id).unwrap();
                    if tag.to_lowercase().contains(&filter) {
                        rows.push(json!({"published":snapshot_json(tag,&snap),"calculated":calculated.snapshot(id).map(|s|snapshot_json(tag,&s))}));
                    }
                }
            } else {
                for ch in value["channels"].as_array().into_iter().flatten() {
                    for dev in ch["devices"].as_array().into_iter().flatten() {
                        for reg in dev["registers"].as_array().into_iter().flatten() {
                            if reg["tag"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&filter)
                            {
                                rows.push(
                                    json!({"tag":reg["tag"],"value":null,"quality":"NotRunning"}),
                                );
                            }
                        }
                    }
                }
            }
            let total = rows.len();
            Ok(
                json!({"running":state.stack.is_some(),"total":total,"offset":offset,"tags":rows.into_iter().skip(offset).take(limit).collect::<Vec<_>>(),"applied_revision":state.applied}),
            )
        }
        "get_tag" => {
            let name = string(&args, "tag")?;
            let reg = value["channels"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|c| c["devices"].as_array().into_iter().flatten())
                .flat_map(|d| d["registers"].as_array().into_iter().flatten())
                .find(|r| r["tag"] == name)
                .ok_or("unknown tag")?;
            let live = state.stack.as_ref().and_then(|s| {
                let store = s.typed();
                store
                    .resolve(name)
                    .and_then(|id| store.snapshot(id))
                    .map(|snap| snapshot_json(name, &snap))
            });
            let calculated = state.stack.as_ref().and_then(|s| {
                let store = s.calculated();
                store
                    .resolve(name)
                    .and_then(|id| store.snapshot(id))
                    .map(|snap| snapshot_json(name, &snap))
            });
            Ok(
                json!({"config":reg,"published":live,"calculated":calculated,"applied_revision":state.applied}),
            )
        }
        "test_formula" => {
            struct Lookup(std::collections::HashMap<String, f64>);
            impl tags_core::TagLookup for Lookup {
                fn numeric_value(&self, name: &str) -> Option<f64> {
                    self.0.get(name).copied()
                }
            }
            let formula = string(&args, "formula")?;
            let input = args["input"].as_f64().ok_or("input must be numeric")?;
            let variable = args["variable"].as_str().unwrap_or("raw");
            if !["raw", "value"].contains(&variable) {
                return Err("variable must be raw or value".into());
            }
            let mut values = std::collections::HashMap::new();
            if let Some(s) = &state.stack {
                let store = s.calculated();
                for (id, snap) in store.snapshot_all() {
                    if snap.quality == mb_poller::Quality::Good {
                        if let Some(v) = snap.value.as_f64() {
                            values.insert(store.name(id).unwrap().to_owned(), v);
                        }
                    }
                }
            }
            if let Some(tags) = args.get("tags") {
                for (name, value) in tags.as_object().ok_or("tags must be an object")? {
                    values.insert(
                        name.clone(),
                        value.as_f64().ok_or("tag preview values must be numbers")?,
                    );
                }
            }
            let compiled =
                tags_core::Formula::compile(formula, variable, Some(Arc::new(Lookup(values))))
                    .map_err(|e| e.to_string())?;
            Ok(
                json!({"result":compiled.eval(variable,input).map_err(|e|e.to_string())?,"dependencies":tags_core::formula::dependencies(formula).map_err(|e|e.to_string())?}),
            )
        }
        "write_tag" => {
            approved(&args)?;
            let stack = state.stack.as_ref().ok_or("server is stopped")?;
            if state.applied.as_deref() != Some(string(&args, "expected_revision")?) {
                return Err("active configuration changed; re-read server_status".into());
            }
            let tag = string(&args, "tag")?;
            let result =
                stack.write_tag(tag, args.get("value").ok_or("value required")?.clone())?;
            Ok(
                json!({"acknowledged":true,"tag":tag,"status":result,"readback":"normal polling will confirm the actual value"}),
            )
        }
        "get_history" => {
            let name = string(&args, "tag")?;
            let limit = number(&args, "limit", 100, 1, 1000)? as usize;
            let history = if let Some(stack) = &state.stack {
                stack.history(name)?
            } else {
                let cfg = validate(&state.desired, &owner.path)?;
                let p = std::path::Path::new(&cfg.gateway.data_dir).join("tags.sled");
                if p.exists() {
                    tags_core::Persist::open(p)
                        .map_err(|e| e.to_string())?
                        .load_history(name)
                        .map_err(|e| e.to_string())?
                } else {
                    vec![]
                }
            };
            let skip = history.len().saturating_sub(limit);
            Ok(json!({"tag":name,"samples":history.into_iter().skip(skip).collect::<Vec<_>>()}))
        }
        "get_logs" => {
            let cfg = validate(&state.desired, &owner.path)?;
            let Some(dir) = cfg.logging.dir else {
                return Ok(
                    json!({"lines":[],"message":"file logging is disabled; use get_audit for MCP operations"}),
                );
            };
            let limit = number(&args, "limit", 100, 1, 200)? as usize;
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&cfg.logging.file_prefix)
                })
                .collect();
            files.sort_by_key(|e| e.file_name());
            let Some(file) = files.last() else {
                return Ok(json!({"lines":[]}));
            };
            let mut file = std::fs::File::open(file.path()).map_err(|e| e.to_string())?;
            let size = file.metadata().map_err(|e| e.to_string())?.len();
            file.seek(SeekFrom::Start(size.saturating_sub(128 * 1024)))
                .map_err(|e| e.to_string())?;
            let mut data = Vec::new();
            file.take(128 * 1024)
                .read_to_end(&mut data)
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&data);
            let lines: Vec<_> = text.lines().collect();
            Ok(
                json!({"lines":lines.into_iter().rev().take(limit).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>() }),
            )
        }
        _ => Err("unknown tool".into()),
    }
}
