from pathlib import Path
p=Path('crates/svc-host/src/mcp/transport.rs');s=p.read_text(encoding='utf-8').replace('''pub(super) fn previous_running(path: &Path) -> bool {
    std::fs::read(sidecar(path))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["running"] == true)
}''','''pub(super) fn previous_running(path: &Path) -> Option<bool> {
    std::fs::read(sidecar(path)).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).and_then(|v|v["running"].as_bool())
}''');p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/mod.rs');s=p.read_text(encoding='utf-8').replace('''pub fn previous_runtime_was_running(path: &Path) -> bool {
    transport::previous_running(path)
}''','''pub fn previous_runtime_was_running(path:&Path)->bool {transport::previous_running(path).unwrap_or(false)}
pub fn previous_runtime_intent(path:&Path)->Option<bool> {transport::previous_running(path)}''');s=s.replace('''            client = None;
        }
        if attached_once''','''            if let Some(intent)=previous_runtime_intent(&path) {running_intent=intent;}
            client = None;
        }
        if attached_once''');p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/server.rs');s=p.read_text(encoding='utf-8').replace('''            let resume = matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Remote(c) if c.running_before_disconnect()));''','''            let remembered=matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Remote(c) if c.running_before_disconnect()));
            let resume=svc_host::mcp::previous_runtime_intent(&path).unwrap_or(remembered);''');p.write_text(s,encoding='utf-8')
