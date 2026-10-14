from pathlib import Path
p=Path('crates/svc-host/src/mcp/protocol.rs');s=p.read_text(encoding='utf-8').replace('pub async fn dispatch(owner: &Arc<Controller>, request: Value) -> Option<Value> {','''pub async fn dispatch(owner: &Arc<Controller>, request: Value) -> Option<Value> {
    dispatch_optional(Some(owner), request).await
}
pub(super) async fn dispatch_optional(owner: Option<&Arc<Controller>>, request: Value) -> Option<Value> {''')
s=s.replace('            match owner.call(name, args).await {','''            let Some(owner) = owner else {
                let message = "Selected configurator/server is offline; launch it with the same configuration path. This MCP bridge never starts another server.";
                return Some(json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":message}],"structuredContent":{"error":message},"isError":true}}));
            };
            match owner.call(name, args).await {''')
s=s.replace('"opcmodbus://config" => owner.call("get_config", json!({})).await,','"opcmodbus://config" => match owner {Some(owner) => owner.call("get_config", json!({})).await, None => Err("Selected configurator/server is offline".into())},').replace('"opcmodbus://status" => owner.call("server_status", json!({})).await,','"opcmodbus://status" => match owner {Some(owner) => owner.call("server_status", json!({})).await, None => Err("Selected configurator/server is offline".into())},')
p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/connection.rs');s=p.read_text(encoding='utf-8');s=s.replace('pub fn connection_info(path: &Path) -> Result<Value, String> {','''pub fn config_id(path: &Path) -> String {
    let text = display(path);
    let text = if cfg!(windows) { text.to_lowercase() } else { text };
    super::fingerprint(text.as_bytes())[..12].to_owned()
}
pub fn connection_info(path: &Path) -> Result<Value, String> {''')
s=s.replace('    let args = json!', '''    let value = super::parse_file(&super::read_file(&path)?)?;
    let instance_name = value.pointer("/gateway/instance_name").and_then(Value::as_str).unwrap_or("OPC Modbus");
    let config_id = config_id(&path);
    let label: String = instance_name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).take(32).collect();
    let server_name = format!("opc_modbus_{label}_{config_id}");
    let args = json!''',1)
s=s.replace('format!("[mcp_servers.opc_modbus]', 'format!("[mcp_servers.{server_name}]')
s=s.replace('json!({"mcpServers":{"opc_modbus":', 'json!({"mcpServers":{(server_name.clone()):')
s=s.replace('json!({"executable":exe,','json!({"config_id":config_id,"instance_name":instance_name,"server_name":server_name,"executable":exe,')
p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/mod.rs');s=p.read_text(encoding='utf-8').replace('pub use connection::{connection_info, default_config_path};','pub use connection::{config_id, connection_info, default_config_path};').replace('pub(super) const OWNER_BUSY:', 'pub const OWNER_BUSY:').replace('mod sessions;', 'mod sessions;\nmod credentials;')
s=s.replace('Client::connect_or_start(&path).await?', 'Client::wait(&path, std::time::Duration::from_secs(3)).await?')
p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/tools.rs');s=p.read_text(encoding='utf-8').replace('json!({"running":state.stack.is_some(),"config_path":owner.path,','json!({"attached":true,"config_id":super::config_id(&owner.path),"instance_name":state.desired.pointer("/gateway/instance_name").and_then(Value::as_str).unwrap_or("OPC Modbus"),"running":state.stack.is_some(),"config_path":owner.path,');p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/client.rs');s=p.read_text(encoding='utf-8');start=s.index('    pub async fn connect_or_start');end=s.index('    pub fn url',start)
s=s[:start]+'''    pub async fn wait(path: impl AsRef<Path>, timeout: Duration) -> Result<Self, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect(&path).await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    if tokio::time::Instant::now() >= deadline { return Err(error); }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
'''+s[end:];p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/transport.rs');s=p.read_text(encoding='utf-8');start=s.index('#[cfg(windows)]\nfn protect_token');end=s.index('fn write_sidecar', start);s=s[:start]+s[end:];s=s.replace('protect_token(tmp.path())?', 'super::credentials::protect(tmp.path(), &owner.path)?')
s=s.replace('''    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|e| format!("MCP endpoint bind failed: {e}"))?;''','''    let listener = match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            tracing::warn!(requested=%address,"MCP port is occupied; selecting a free port for this configuration");
            tokio::net::TcpListener::bind(SocketAddr::new(address.ip(), 0)).await
                .map_err(|e|format!("MCP endpoint bind failed: {e}"))?
        }
        Err(e) => return Err(format!("MCP endpoint bind failed: {e}")),
    };''')
s=s.replace('serde_json::from_slice(&std::fs::read(sidecar(path)).map_err(|e| e.to_string())?)','serde_json::from_slice(&std::fs::read(sidecar(path)).map_err(|e| format!("Cannot read MCP connection for {}: {e}", path.display()))?)')
start=s.index('pub(super) async fn connect_stdio');s=s[:start]+'''async fn bridge_connect(path: &Path, previous: Option<&Endpoint>, initialize: Option<&Value>, session: &str) -> Result<Endpoint, String> {
    let endpoint = read_endpoint(path)?;
    let status = remote(&endpoint, &json!({"jsonrpc":"2.0","id":0,"method":"tools/call","params":{"name":"server_status","arguments":{}}})).await?.ok_or("Empty controller status")?;
    if status.pointer("/result/structuredContent/instance_id").and_then(Value::as_str) != Some(&endpoint.instance) {
        return Err("Selected controller changed; waiting for its current endpoint".into());
    }
    if let Some(initialize) = initialize {
        if previous.is_none_or(|old|old.instance != endpoint.instance) {
            remote_session(&endpoint, initialize, Some(session)).await?;
        } else {
            remote_session(&endpoint, &json!({"jsonrpc":"2.0","method":"notifications/bridge/heartbeat"}), Some(session)).await?;
        }
    }
    Ok(endpoint)
}
fn offline_status(path: &Path, error: &str) -> Value {
    let name = super::read_file(path).ok().and_then(|bytes|super::parse_file(&bytes).ok())
        .and_then(|v|v.pointer("/gateway/instance_name").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(||"OPC Modbus".into());
    json!({"attached":false,"running":false,"config_path":path,"config_id":super::config_id(path),"instance_name":name,"mcp_clients":[],"last_error":format!("Waiting for selected configurator/server: {error}")})
}
pub async fn stdio(path: PathBuf) -> Result<(), String> {
    let path = std::fs::canonicalize(path).map_err(|e|e.to_string())?;
    let session = random_id()?;
    let mut connected: Option<Endpoint> = None;
    let mut initialize: Option<Value> = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut output = tokio::io::stdout();
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(1));
    let result = async {
        loop {
            let line = tokio::select! {
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
                    let local = matches!(value["method"].as_str(),Some("initialize"|"ping"|"tools/list"|"resources/list"|"resources/templates/list"));
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
                output.write_all(b"\\n").await.map_err(|e|e.to_string())?;
                output.flush().await.map_err(|e|e.to_string())?;
            }
        }
        Ok::<(),String>(())
    }.await;
    if let Some(endpoint) = connected {
        let _ = remote_session(&endpoint,&json!({"jsonrpc":"2.0","method":"notifications/bridge/disconnected"}),Some(&session)).await;
    }
    result
}
'''
p.write_text(s,encoding='utf-8')
