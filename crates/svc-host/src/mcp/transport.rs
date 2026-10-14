use super::{protocol, random_id, Controller, MAX_CONFIG_BYTES};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use http_body_util::{BodyExt, Full};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Clone)]
pub(super) struct Endpoint {
    pub url: String,
    pub token: String,
    pub instance: String,
    pub address: SocketAddr,
}
fn sidecar(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".mcp-endpoint.json");
    PathBuf::from(p)
}
pub(super) fn write_sidecar(owner: &Controller, endpoint: &Endpoint) -> Result<(), String> {
    use std::io::Write;
    let mut tmp =
        tempfile::NamedTempFile::new_in(owner.path.parent().unwrap()).map_err(|e| e.to_string())?;
    super::credentials::protect(tmp.path(), &owner.path)?;
    let value = json!({"url":endpoint.url,"token":endpoint.token,"instance_id":endpoint.instance,"config":owner.path,"pid":std::process::id(),"running":owner.view().running});
    tmp.write_all(value.to_string().as_bytes())
        .map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    tmp.persist(sidecar(&owner.path))
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub(super) fn previous_running(path: &Path) -> Option<bool> {
    std::fs::read(sidecar(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["running"].as_bool())
}
pub(super) fn stop_marker(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".mcp-stop-request.json");
    PathBuf::from(name)
}
pub(super) fn mark_stop(owner: &Controller) -> Result<(), String> {
    use std::io::Write;
    let mut tmp =
        tempfile::NamedTempFile::new_in(owner.path.parent().unwrap()).map_err(|e| e.to_string())?;
    super::credentials::protect(tmp.path(), &owner.path)?;
    tmp.write_all(b"{\"requested\":true}")
        .map_err(|e| e.to_string())?;
    tmp.persist(stop_marker(&owner.path))
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub(super) fn remove_endpoint(owner: &Controller) {
    let path = sidecar(&owner.path);
    if let (Some(endpoint), Ok(bytes)) = (owner.endpoint.read().as_ref(), std::fs::read(&path)) {
        if serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["instance_id"].as_str().map(str::to_owned))
            .as_deref()
            == Some(&endpoint.instance)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}
pub(super) async fn listen(owner: &Arc<Controller>) -> Result<(), String> {
    if !owner.settings.enabled {
        return Ok(());
    }
    let address = SocketAddr::new(
        owner.settings.host.parse().map_err(|_| "invalid MCP IP")?,
        owner.settings.port,
    );
    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!(requested=%address,"MCP port is occupied; selecting a free port for this configuration");
            tokio::net::TcpListener::bind(SocketAddr::new(address.ip(), 0))
                .await
                .map_err(|e| format!("MCP endpoint bind failed: {e}"))?
        }
        Err(e) => return Err(format!("MCP endpoint bind failed: {e}")),
    };
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let token = std::fs::read(sidecar(&owner.path))
        .ok()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        .and_then(|v| {
            v["token"]
                .as_str()
                .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(str::to_owned)
        })
        .map(Ok)
        .unwrap_or_else(random_id)?;
    let endpoint = Endpoint {
        url: format!("http://{address}/mcp"),
        token,
        instance: random_id()?,
        address,
    };
    write_sidecar(owner, &endpoint)?;
    *owner.endpoint.write() = Some(endpoint);
    let app = Router::new()
        .route("/mcp", post(http_request).get(no_stream).delete(disconnect))
        .layer(DefaultBodyLimit::max(MAX_CONFIG_BYTES))
        .with_state(Arc::clone(owner));
    let mut exit = owner.exit.subscribe();
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                if !*exit.borrow() {
                    let _ = exit.changed().await;
                }
            })
            .await
        {
            tracing::error!(error=%e,"MCP HTTP stopped");
        }
    });
    owner.tasks.lock().push(task);
    Ok(())
}
fn authorized(owner: &Controller, headers: &HeaderMap) -> bool {
    let guard = owner.endpoint.read();
    let Some(endpoint) = guard.as_ref() else {
        return false;
    };
    let allowed_host = endpoint.address.to_string();
    let localhost = format!("localhost:{}", endpoint.address.port());
    let host = headers.get("host").and_then(|h| h.to_str().ok());
    if host != Some(allowed_host.as_str()) && host != Some(localhost.as_str()) {
        return false;
    }
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().unwrap_or("");
        if origin != format!("http://{allowed_host}") && origin != format!("http://{localhost}") {
            return false;
        }
    }
    let supplied = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    supplied.len() == endpoint.token.len()
        && supplied
            .bytes()
            .zip(endpoint.token.bytes())
            .fold(0u8, |v, (a, b)| v | (a ^ b))
            == 0
}
async fn no_stream(State(owner): State<Arc<Controller>>, headers: HeaderMap) -> Response {
    if !authorized(&owner, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
async fn disconnect(State(owner): State<Arc<Controller>>, headers: HeaderMap) -> Response {
    if !authorized(&owner, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(id) = headers.get("mcp-session-id").and_then(|h| h.to_str().ok()) {
        owner.clients.lock().remove(id);
    }
    StatusCode::NO_CONTENT.into_response()
}
async fn http_request(
    State(owner): State<Arc<Controller>>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    if !authorized(&owner, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if let Some(version) = headers
        .get("mcp-protocol-version")
        .and_then(|h| h.to_str().ok())
    {
        if !protocol::VERSIONS.contains(&version) {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    if !headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let request: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(protocol::error(Value::Null, -32700, "JSON parse error")),
            )
                .into_response()
        }
    };
    let bridge = headers.get("x-mcp-bridge-id").and_then(|h| h.to_str().ok());
    let session = bridge.or_else(|| headers.get("mcp-session-id").and_then(|h| h.to_str().ok()));
    // These notifications belong to the authenticated local stdio bridge.
    if bridge.is_some() && request["method"] == "notifications/bridge/disconnected" {
        owner.clients.lock().remove(bridge.unwrap());
        return StatusCode::ACCEPTED.into_response();
    }
    if bridge.is_some() && request["method"] == "notifications/bridge/heartbeat" {
        return if owner.touch_client(bridge.unwrap()) {
            StatusCode::ACCEPTED
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response();
    }
    let initializing = request["method"] == "initialize";
    let info = request
        .pointer("/params/clientInfo")
        .cloned()
        .unwrap_or(json!({}));
    let reply = protocol::dispatch(&owner, request).await;
    let mut response = match &reply {
        Some(v) => Json(v.clone()).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    };
    if initializing && reply.as_ref().is_some_and(|r| r.get("result").is_some()) {
        let id = match session {
            Some(id) => id.to_owned(),
            None => match random_id() {
                Ok(id) => id,
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            },
        };
        owner.register_client(&id, &info, if bridge.is_some() { "stdio" } else { "HTTP" });
        if let Ok(value) = id.parse() {
            response.headers_mut().insert("mcp-session-id", value);
        }
    } else if let Some(id) = session {
        owner.touch_client(id);
    }
    response
}
pub(super) async fn remote(endpoint: &Endpoint, request: &Value) -> Result<Option<Value>, String> {
    remote_session(endpoint, request, None).await
}
async fn remote_session(
    endpoint: &Endpoint,
    request: &Value,
    session: Option<&str>,
) -> Result<Option<Value>, String> {
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        remote_session_inner(endpoint, request, session),
    )
    .await
    .map_err(|_| "MCP request timeout".to_owned())?
}
async fn remote_session_inner(
    endpoint: &Endpoint,
    request: &Value,
    session: Option<&str>,
) -> Result<Option<Value>, String> {
    use hyper_util::rt::TokioIo;
    let stream = tokio::time::timeout(
        std::time::Duration::from_millis(750),
        tokio::net::TcpStream::connect(endpoint.address),
    )
    .await
    .map_err(|_| "MCP connect timeout")?
    .map_err(|e| e.to_string())?;
    let (mut client, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut builder = hyper::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", endpoint.address.to_string())
        .header("Authorization", format!("Bearer {}", endpoint.token))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25");
    if let Some(id) = session {
        builder = builder.header("x-mcp-bridge-id", id);
    }
    let req = builder
        .body(Full::new(bytes::Bytes::from(request.to_string())))
        .map_err(|e| e.to_string())?;
    let result = async {
        let reply = client.send_request(req).await.map_err(|e| e.to_string())?;
        if reply.status() == StatusCode::ACCEPTED {
            return Ok(None);
        }
        if !reply.status().is_success() {
            return Err(format!("MCP HTTP {}", reply.status()));
        }
        let bytes = reply
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| e.to_string())
    }
    .await;
    task.abort();
    result
}
pub(super) fn read_endpoint(path: &Path) -> Result<Endpoint, String> {
    let value: Value = serde_json::from_slice(
        &std::fs::read(sidecar(path))
            .map_err(|e| format!("Cannot read MCP connection for {}: {e}", path.display()))?,
    )
    .map_err(|e| e.to_string())?;
    if value["config"].as_str() != path.to_str() {
        return Err("MCP endpoint belongs to another configuration".into());
    }
    let url = value["url"].as_str().ok_or("MCP URL missing")?.to_owned();
    let address: SocketAddr = url
        .strip_prefix("http://")
        .and_then(|s| s.strip_suffix("/mcp"))
        .ok_or("invalid local MCP URL")?
        .parse()
        .map_err(|_| "invalid MCP address")?;
    if !address.ip().is_loopback() {
        return Err("MCP bridge only connects to loopback".into());
    }
    Ok(Endpoint {
        url,
        address,
        token: value["token"].as_str().ok_or("MCP token missing")?.into(),
        instance: value["instance_id"]
            .as_str()
            .ok_or("MCP instance missing")?
            .into(),
    })
}
async fn bridge_connect(
    path: &Path,
    previous: Option<&Endpoint>,
    initialize: Option<&Value>,
    session: &str,
) -> Result<Endpoint, String> {
    let endpoint = read_endpoint(path)?;
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        remote(&endpoint, &json!({"jsonrpc":"2.0","id":0,"method":"ping"})),
    )
    .await
    .map_err(|_| "Selected controller is not responding".to_owned())??
    .ok_or("Empty controller ping")?;
    if reply.get("result").is_none() {
        return Err("Selected controller rejected MCP ping".into());
    }
    if let Some(initialize) = initialize {
        if previous.is_none_or(|old| old.instance != endpoint.instance) {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                remote_session(&endpoint, initialize, Some(session)),
            )
            .await
            .map_err(|_| "MCP initialization timeout")??;
        } else {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                remote_session(
                    &endpoint,
                    &json!({"jsonrpc":"2.0","method":"notifications/bridge/heartbeat"}),
                    Some(session),
                ),
            )
            .await
            .map_err(|_| "MCP heartbeat timeout")??;
        }
    }
    Ok(endpoint)
}
fn offline_status(path: &Path, error: &str) -> Value {
    let name = super::read_file(path)
        .ok()
        .and_then(|bytes| super::parse_file(&bytes).ok())
        .and_then(|v| {
            v.pointer("/gateway/instance_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "OPC Modbus".into());
    json!({"attached":false,"running":false,"config_path":path,"config_id":super::config_id(path),"instance_name":name,"mcp_clients":[],"last_error":format!("Waiting for selected configurator/server: {error}")})
}
pub async fn stdio(path: PathBuf) -> Result<(), String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let session = random_id()?;
    let mut connected: Option<Endpoint> = None;
    let mut initialize: Option<Value> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async {
        loop {
            let line = tokio::select! {
                biased;
                line = lines.next_line() => match line.map_err(|e|e.to_string())? {Some(line) => line, None => break},
                _ = heartbeat.tick() => {
                    if initialize.is_some() {
                        connected = bridge_connect(&path, connected.as_ref(), initialize.as_ref(), &session).await.ok();
                    }
                    continue;
                }
            };
            if line.trim().is_empty() {continue;}
            let request = if line.len() > MAX_CONFIG_BYTES {Err("message too large".to_owned())}
                else {serde_json::from_str::<Value>(&line).map_err(|e|e.to_string())};
            let response = match request {
                Err(_) => Some(protocol::error(Value::Null,-32700,"JSON parse error or oversized message")),
                Ok(value) => {
                    let local = matches!(value["method"].as_str(),Some("initialize"|"ping"|"tools/list"|"resources/list"|"resources/templates/list"|"notifications/initialized"));
                    if local {
                        let reply = protocol::dispatch_optional(None,value.clone()).await;
                        if value["method"] == "initialize" && reply.as_ref().is_some_and(|r|r.get("result").is_some()) {
                            initialize = Some(value);
                            // Initialization never depends on GUI/server readiness.
                            connected = None;
                        }
                        reply
                    } else {
                        match bridge_connect(&path,connected.as_ref(),initialize.as_ref(),&session).await {
                            Ok(endpoint) => {
                                // Transport failures never replay a possibly mutating tool.
                                let reply = remote_session(&endpoint,&value,Some(&session)).await;
                                connected = Some(endpoint);
                                match reply {
                                    Ok(reply) => reply,
                                    Err(e) => value.get("id").map(|id|protocol::error(id.clone(),-32603,&e)),
                                }
                            }
                            Err(e) => {
                                connected = None;
                                if value["method"] == "tools/call" && value.pointer("/params/name").and_then(Value::as_str)==Some("server_status") {
                                    value.get("id").map(|id| {let status=offline_status(&path,&e);json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":status.to_string()}],"structuredContent":status,"isError":false}})})
                                } else {protocol::dispatch_optional(None,value).await}
                            }
                        }
                    }
                }
            };
            if let Some(value) = response {
                output.write_all(value.to_string().as_bytes()).await.map_err(|e|e.to_string())?;
                output.write_all(b"\n").await.map_err(|e|e.to_string())?;
                output.flush().await.map_err(|e|e.to_string())?;
            }
        }
        Ok::<(),String>(())
    }.await;
    if let Some(endpoint) = connected {
        let _ = remote_session(
            &endpoint,
            &json!({"jsonrpc":"2.0","method":"notifications/bridge/disconnected"}),
            Some(&session),
        )
        .await;
    }
    result
}
