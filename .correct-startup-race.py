from pathlib import Path
p=Path('crates/svc-host/src/mcp/transport.rs');s=p.read_text(encoding='utf-8').replace('fn write_sidecar(owner:', 'pub(super) fn write_sidecar(owner:',1).replace('"pid":std::process::id()','"pid":std::process::id(),"running":owner.view().running',1)
pos=s.index('pub(super) fn stop_marker');s=s[:pos]+'''pub(super) fn previous_running(path: &Path) -> bool {
    std::fs::read(sidecar(path)).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v|v["running"]==true)
}
'''+s[pos:];p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/mod.rs');s=p.read_text(encoding='utf-8');s=s.replace('''    fn refresh_view(&self, state: &State) {
        let mut view''','''    fn refresh_view(&self, state: &State) {
        let was_running=self.view.read().running;
        let mut view''');s=s.replace('''        *self.view.write() = view;
    }
    pub fn view''','''        let running=view.running;
        *self.view.write() = view;
        if running!=was_running {
            let endpoint=self.endpoint.read().clone();
            if let Some(endpoint)=endpoint {
                if let Err(error)=transport::write_sidecar(self,&endpoint) {tracing::error!(%error,"Cannot publish controller running state");}
            }
        }
    }
    pub fn view''');s=s.replace('pub fn shutdown_was_requested(path:', 'pub fn previous_runtime_was_running(path:&Path)->bool {transport::previous_running(path)}\npub fn shutdown_was_requested(path:',1);p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/server.rs');s=p.read_text(encoding='utf-8');s=s.replace('''async fn attach_controller(path: std::path::PathBuf) -> Result<GuiStack, String> {
    match''','''async fn attach_controller(path: std::path::PathBuf) -> Result<GuiStack, String> {
    let resume=svc_host::mcp::previous_runtime_was_running(&path) && !svc_host::mcp::shutdown_was_requested(&path);
    match''');s=s.replace('''        Ok(controller) => Ok(GuiStack::Managed(controller)),''','''        Ok(controller) => {
            if resume {
                if let Err(error)=controller.start_server().await {controller.shutdown().await;return Err(error);}
            }
            Ok(GuiStack::Managed(controller))
        }''',1);p.write_text(s,encoding='utf-8')
