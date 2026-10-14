use super::{transport, OpcUaView, ServerView};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    sync::Arc,
    time::{Duration, Instant, UNIX_EPOCH},
};
use tags_core::TypedReader;

#[derive(Clone)]
pub struct Client {
    path: PathBuf,
    endpoint: transport::Endpoint,
}
impl Client {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
        let endpoint = transport::read_endpoint(&path)?;
        let client = Self { path, endpoint };
        let status = client.call("server_status", json!({})).await?;
        if status["instance_id"] != client.endpoint.instance {
            return Err("MCP owner changed; reconnect".into());
        }
        Ok(client)
    }
    pub async fn wait(path: impl AsRef<Path>, timeout: Duration) -> Result<Self, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match Self::connect(&path).await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
    pub fn instance_id(&self) -> &str {
        &self.endpoint.instance
    }
    pub fn url(&self) -> String {
        self.endpoint.url.clone()
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let response=tokio::time::timeout(Duration::from_secs(15),transport::remote(&self.endpoint,&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))).await.map_err(|_|"MCP request timeout")??.ok_or("empty MCP response")?;
        if let Some(error) = response.get("error") {
            return Err(error.to_string());
        }
        let result = &response["result"];
        if result["isError"] == true {
            return Err(result["structuredContent"]["error"]
                .as_str()
                .unwrap_or("MCP tool failed")
                .into());
        }
        Ok(result["structuredContent"].clone())
    }
    pub async fn view(&self) -> Result<ServerView, String> {
        let status = self.call("server_status", json!({})).await?;
        let mut rows = vec![];
        let mut offset = 0;
        loop {
            let page = self
                .call("read_tags", json!({"offset":offset,"limit":1000}))
                .await?;
            let part = page["tags"].as_array().cloned().unwrap_or_default();
            let count = part.len();
            rows.extend(part);
            offset += count;
            if count == 0 || offset >= page["total"].as_u64().unwrap_or(0) as usize {
                break;
            }
        }
        let names: Vec<_> = rows
            .iter()
            .filter_map(|r| r["published"]["tag"].as_str().or_else(|| r["tag"].as_str()))
            .collect();
        let store = Arc::new(tags_core::TypedStore::from_names(names.iter().copied()));
        for row in &rows {
            let s = &row["published"];
            let Some(name) = s["tag"].as_str() else {
                continue;
            };
            let Some(id) = store.resolve(name) else {
                continue;
            };
            let v = &s["value"];
            let value = if s["value_type"] == "UInt" {
                tags_core::TypedValue::UInt(v.as_u64().unwrap_or(0))
            } else if s["value_type"] == "Int" {
                tags_core::TypedValue::Int(v.as_i64().unwrap_or(0))
            } else if let Some(b) = v.as_bool() {
                tags_core::TypedValue::Bool(b)
            } else if let Some(f) = v.as_f64() {
                tags_core::TypedValue::Float(f)
            } else if let Some(t) = v.as_str() {
                tags_core::TypedValue::Text(t.into())
            } else if let Some(a) = v.as_array() {
                tags_core::TypedValue::Bytes(
                    a.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect(),
                )
            } else {
                tags_core::TypedValue::Absent
            };
            let quality = match s["quality"].as_str() {
                Some("Good") => mb_poller::Quality::Good,
                Some("Uncertain") => mb_poller::Quality::Uncertain,
                _ => mb_poller::Quality::Bad,
            };
            store.seed(
                id,
                tags_core::TypedSnapshot {
                    value,
                    quality,
                    source_ts: UNIX_EPOCH
                        + Duration::from_millis(s["source_timestamp_ms"].as_u64().unwrap_or(0)),
                    mono: Instant::now()
                        .checked_sub(Duration::from_millis(s["age_ms"].as_u64().unwrap_or(0)))
                        .unwrap_or_else(Instant::now),
                    seq: s["sequence"].as_u64().unwrap_or(0),
                },
            );
        }
        let mut metrics = vec![];
        for row in status["channels"].as_array().into_iter().flatten() {
            let m = Arc::new(mb_poller::ChannelMetrics::default());
            m.connected
                .store(row["connected"] == true, Ordering::Relaxed);
            *m.last_error.write() = row["last_error"].as_str().map(str::to_owned);
            for (field, key) in [
                (&m.reqs_ok, "requests_ok"),
                (&m.reqs_err, "requests_error"),
                (&m.timeouts, "timeouts"),
                (&m.exceptions, "exceptions"),
                (&m.protocol_errors, "protocol_errors"),
                (&m.reconnects, "reconnects"),
                (&m.writes_ok, "writes_ok"),
                (&m.writes_err, "writes_error"),
            ] {
                field.store(row[key].as_u64().unwrap_or(0), Ordering::Relaxed);
            }
            metrics.push((row["channel"].as_str().unwrap_or("").into(), m));
        }
        Ok(ServerView {
            mcp_clients: serde_json::from_value(status["mcp_clients"].clone()).unwrap_or_default(),
            saved_revision: status["revision"].as_str().map(str::to_owned),
            applied_revision: status["applied_revision"].as_str().map(str::to_owned),
            running: status["running"] == true,
            revision: status["runtime_revision"].as_u64().unwrap_or(0),
            typed: Some(store),
            metrics,
            endpoint: status["opcua_endpoint"].as_str().map(str::to_owned),
            opcua: status["opcua_sessions"].as_u64().map(|n| OpcUaView {
                sessions: n as usize,
                authentications: vec![],
            }),
            error: status["last_error"].as_str().map(str::to_owned),
            trial_remaining: None,
        })
    }
}
