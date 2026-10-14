from pathlib import Path
p=Path('crates/svc-host/src/mcp/transport.rs');s=p.read_text(encoding='utf-8');pos=s.index('pub(super) fn remove_endpoint')
s=s[:pos]+'''pub(super) fn stop_marker(path: &Path) -> PathBuf {
    let mut name=path.as_os_str().to_os_string(); name.push(".mcp-stop-request.json"); PathBuf::from(name)
}
pub(super) fn mark_stop(owner: &Controller) -> Result<(),String> {
    use std::io::Write;
    let mut tmp=tempfile::NamedTempFile::new_in(owner.path.parent().unwrap()).map_err(|e|e.to_string())?;
    super::credentials::protect(tmp.path(),&owner.path)?;
    tmp.write_all(b"{\\"requested\\":true}").map_err(|e|e.to_string())?;
    tmp.persist(stop_marker(&owner.path)).map_err(|e|e.to_string())?;
    Ok(())
}
'''+s[pos:]
s=s.replace('''            remote_session(&endpoint, initialize, Some(session)).await?;''','''            tokio::time::timeout(std::time::Duration::from_secs(1),remote_session(&endpoint, initialize, Some(session))).await.map_err(|_|"MCP initialization timeout")??;''')
s=s.replace('''            remote_session(
                &endpoint,
                &json!({"jsonrpc":"2.0","method":"notifications/bridge/heartbeat"}),
                Some(session),
            )
            .await?;''','''            tokio::time::timeout(std::time::Duration::from_secs(1),remote_session(
                &endpoint,
                &json!({"jsonrpc":"2.0","method":"notifications/bridge/heartbeat"}),
                Some(session),
            )).await.map_err(|_|"MCP heartbeat timeout")??;''')
s=s.replace('''"resources/templates/list"
                            )''','''"resources/templates/list" | "notifications/initialized"
                            )''')
p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/mod.rs');s=p.read_text(encoding='utf-8');pos=s.index('pub async fn run_managed(');end=s.index('pub async fn run_controller(',pos)
s=s[:pos]+'''pub fn shutdown_was_requested(path: &Path) -> bool {
    std::fs::read(transport::stop_marker(path)).ok().and_then(|b|serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v|v["requested"]==true)
}
pub async fn run_managed(path: PathBuf, shutdown: impl std::future::Future<Output=()>) -> Result<(),String> {
    tokio::pin!(shutdown);
    let mut client: Option<Client> = None;
    let mut attached_once=false;
    let mut tick=tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if let Some(current)=&client {
            tokio::select! {
                _ = &mut shutdown => {
                    current.call("server_stop",json!({"confirm":true})).await?;
                    return Ok(());
                }
                _ = tick.tick() => {}
            }
            if current.call("server_status",json!({})).await.is_ok() {continue;}
            client=None;
        }
        if attached_once && shutdown_was_requested(&path) {return Ok(());}
        match Controller::new(&path).await {
            Ok(controller) => {
                if let Err(error)=controller.start_server().await {controller.shutdown().await;return Err(error);}
                let mut exit=controller.exit.subscribe();
                tokio::select! {_ = &mut shutdown => {}, _ = exit.changed() => {}}
                controller.shutdown().await;
                return Ok(());
            }
            Err(error) if error==OWNER_BUSY => {
                match Client::wait(&path,std::time::Duration::from_millis(500)).await {
                    Ok(current) => {
                        if !attached_once {current.call("server_start",json!({})).await?;}
                        attached_once=true; client=Some(current);
                    }
                    Err(error) => {
                        if error.contains("os error 5") {return Err(error);}
                        tokio::select! {_ = &mut shutdown => return Ok(()), _ = tick.tick() => {}}
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
}
'''+s[end:]
s=s.replace('''        transport::listen(&controller).await?;''','''        transport::listen(&controller).await?;
        let _ = std::fs::remove_file(transport::stop_marker(&controller.path));''',1)
p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/tools.rs');s=p.read_text(encoding='utf-8').replace('''            approved(&args)?;
            owner.closed.store(true, Ordering::Release);''','''            approved(&args)?;
            super::transport::mark_stop(owner)?;
            owner.closed.store(true, Ordering::Release);''');p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/remote.rs');s=p.read_text(encoding='utf-8').replace('    connected: std::sync::atomic::AtomicBool,','    connected: std::sync::atomic::AtomicBool,\n    running_intent: std::sync::atomic::AtomicBool,').replace('''            connected: std::sync::atomic::AtomicBool::new(true),''','''            connected: std::sync::atomic::AtomicBool::new(true),
            running_intent: std::sync::atomic::AtomicBool::new(view.running),''');s=s.replace('            view: parking_lot::RwLock::new(view),','            view: parking_lot::RwLock::new(view.clone()),')
s=s.replace('''                    Ok(view) => {
                        owner''','''                    Ok(view) => {
                        owner.running_intent.store(view.running, std::sync::atomic::Ordering::Relaxed);
                        owner''');s=s.replace('    pub fn is_connected(&self) -> bool {','''    pub fn running_before_disconnect(&self) -> bool {self.running_intent.load(std::sync::atomic::Ordering::Relaxed)}
    pub fn is_connected(&self) -> bool {''');s=s.replace('''        client.call("server_start", json!({})).await?;''','''        client.call("server_start", json!({})).await?;
        self.running_intent.store(true,std::sync::atomic::Ordering::Relaxed);''');s=s.replace('''        client.call("server_stop", json!({"confirm":true})).await?;''','''        client.call("server_stop", json!({"confirm":true})).await?;
        self.running_intent.store(false,std::sync::atomic::Ordering::Relaxed);''');p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/server.rs');s=p.read_text(encoding='utf-8').replace('    watch_path: Option<std::path::PathBuf>,','    watch_path: Option<std::path::PathBuf>,\n    auto_attach: bool,',1).replace('            watch_path: None,','            watch_path: None,\n            auto_attach: false,',1)
s=s.replace('''    pub fn attach(&mut self, path: std::path::PathBuf) {
        self.shutdown_blocking();''','''    pub fn attach(&mut self, path: std::path::PathBuf) {self.attach_runtime(path,false);}
    fn attach_runtime(&mut self,path:std::path::PathBuf,resume:bool) {
        self.auto_attach=true;
        self.shutdown_blocking();''')
s=s.replace('''                    Ok(stack) => Ev::Started(Box::new(stack), Instant::now()),''','''                    Ok(stack) => {
                        if resume {
                            let result=match &stack {GuiStack::Managed(c)=>c.start_server().await,GuiStack::Remote(c)=>c.start().await,_=>unreachable!()};
                            if let Err(error)=result {stack.shutdown().await;return Ev::StartFailed(error);}
                        }
                        Ev::Started(Box::new(stack),Instant::now())
                    }''',1)
s=s.replace('''        let retry = matches!(self.state, ServerState::Stopped)''','''        let retry = self.auto_attach && matches!(self.state, ServerState::Stopped)''')
s=s.replace('''        if (lost || retry) && self.watch_path.is_some() {
            let path = self.watch_path.clone().unwrap();
            self.last_check = Instant::now();
            self.attach(path);''','''        if (lost || retry) && self.watch_path.is_some() {
            let path = self.watch_path.clone().unwrap();
            if svc_host::mcp::shutdown_was_requested(&path) {
                self.shutdown_blocking(); self.auto_attach=false;
                self.last_error=Some("Контроллер завершён командой MCP. Запустите сервер для повторного подключения.".into());
                return true;
            }
            let resume=matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Remote(c) if c.running_before_disconnect()));
            self.last_check = Instant::now();
            self.attach_runtime(path,resume);''')
p.write_text(s,encoding='utf-8')
