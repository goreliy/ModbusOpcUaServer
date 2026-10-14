use super::{now_ms, Controller};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpClientView {
    pub id: String,
    pub name: String,
    pub version: String,
    pub transport: String,
    pub connected_at_ms: u128,
    pub last_seen_ms: u128,
}
pub(super) struct Session {
    pub view: McpClientView,
    pub seen: Instant,
}
impl Controller {
    pub fn client_views(&self) -> Vec<McpClientView> {
        let mut clients = self.clients.lock();
        clients.retain(|_, s| s.seen.elapsed() < Duration::from_secs(60));
        let mut views: Vec<_> = clients.values().map(|s| s.view.clone()).collect();
        views.sort_by(|a, b| a.id.cmp(&b.id));
        views
    }
    pub(super) fn register_client(&self, id: &str, info: &serde_json::Value, transport: &str) {
        let now = now_ms();
        self.clients.lock().insert(
            id.into(),
            Session {
                view: McpClientView {
                    id: id.into(),
                    name: info["name"]
                        .as_str()
                        .unwrap_or("MCP client")
                        .chars()
                        .take(128)
                        .collect(),
                    version: info["version"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(64)
                        .collect(),
                    transport: transport.into(),
                    connected_at_ms: now,
                    last_seen_ms: now,
                },
                seen: Instant::now(),
            },
        );
    }
    pub(super) fn touch_client(&self, id: &str) -> bool {
        let mut clients = self.clients.lock();
        if let Some(session) = clients.get_mut(id) {
            session.seen = Instant::now();
            session.view.last_seen_ms = now_ms();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn tracks_independent_clients_and_expires_dead_bridges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, json!({"schema_version":"1","mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
        let owner = Controller::new(&path).await.unwrap();
        owner.register_client("a", &json!({"name":"Codex","version":"1"}), "stdio");
        owner.register_client("b", &json!({"name":"Claude","version":"2"}), "HTTP");
        assert_eq!(owner.client_views().len(), 2);
        owner.clients.lock().get_mut("a").unwrap().seen = Instant::now() - Duration::from_secs(61);
        assert!(owner.touch_client("b"));
        let views = owner.client_views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].name, "Claude");
        assert!(!owner.touch_client("a"));
        owner.shutdown().await;
    }
}
