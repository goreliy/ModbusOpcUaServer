use super::Controller;
use serde_json::{json, Value};
use std::sync::Arc;

pub const VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
pub fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
pub async fn dispatch(owner: &Arc<Controller>, request: Value) -> Option<Value> {
    dispatch_optional(Some(owner), request).await
}
pub(super) async fn dispatch_optional(
    owner: Option<&Arc<Controller>>,
    request: Value,
) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str);
    if !request.is_object()
        || request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || method.is_none()
        || id
            .as_ref()
            .is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
    {
        return Some(error(Value::Null, -32600, "invalid JSON-RPC request"));
    }
    let method = method.unwrap();
    let id = id?;
    let params = request.get("params").cloned().unwrap_or(json!({}));
    if !params.is_object() {
        return Some(error(id, -32602, "params must be an object"));
    }
    let result = match method {
        "initialize" => {
            let Some(requested) = params.get("protocolVersion").and_then(Value::as_str) else {
                return Some(error(id, -32602, "protocolVersion required"));
            };
            let version = if VERSIONS.contains(&requested) {
                requested
            } else {
                "2025-11-25"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false},"resources":{}},
                "serverInfo":{"name":"opc-modbus-server","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Manage only this OPC Modbus instance. Read get_config first and supply expected_revision for edits. Device writes, stop/restart and shutdown require explicit human approval of the exact action before setting confirm=true. Discovery uses standard reads and never guesses a device model. Use scan_status/cancel_scan for background scans."})
        }
        "ping" => json!({}),
        "resources/templates/list" => json!({"resourceTemplates":[]}),
        "tools/list" => json!({"tools":super::tools::definitions()}),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tool name required"));
            };
            if !super::tools::known(name) {
                return Some(error(id, -32602, "unknown tool"));
            }
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return Some(error(id, -32602, "arguments must be an object"));
            }
            let Some(owner) = owner else {
                let message = "Selected configurator/server is offline; launch it with the same configuration path. This MCP bridge never starts another server.";
                return Some(
                    json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":message}],"structuredContent":{"error":message},"isError":true}}),
                );
            };
            match owner.call(name, args).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(e) => {
                    json!({"content":[{"type":"text","text":e}],"structuredContent":{"error":e},"isError":true})
                }
            }
        }
        "resources/list" => json!({"resources":[
            {"uri":"opcmodbus://config","name":"Configuration (secrets redacted)","mimeType":"application/json"},
            {"uri":"opcmodbus://status","name":"Live server status","mimeType":"application/json"},
            {"uri":"opcmodbus://examples","name":"Configuration and tool examples","mimeType":"application/json"}
        ]}),
        "resources/read" => {
            let Some(uri) = params.get("uri").and_then(Value::as_str) else {
                return Some(error(id, -32602, "uri required"));
            };
            let value = match uri {
                "opcmodbus://config" => match owner {
                    Some(owner) => owner.call("get_config", json!({})).await,
                    None => Err("Selected configurator/server is offline".into()),
                },
                "opcmodbus://status" => match owner {
                    Some(owner) => owner.call("server_status", json!({})).await,
                    None => Err("Selected configurator/server is offline".into()),
                },
                "opcmodbus://examples" => Ok(super::tools::examples()),
                _ => return Some(error(id, -32602, "unknown resource")),
            };
            match value {
                Ok(value) => {
                    json!({"contents":[{"uri":uri,"mimeType":"application/json","text":value.to_string()}]})
                }
                Err(e) => return Some(error(id, -32603, &e)),
            }
        }
        _ => return Some(error(id, -32601, "method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
