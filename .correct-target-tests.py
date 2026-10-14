from pathlib import Path
p=Path('crates/gui/src/remote.rs');s=p.read_text(encoding='utf-8').replace('    stop: watch::Sender<bool>,','    stop: watch::Sender<bool>,\n    connected: std::sync::atomic::AtomicBool,').replace('            stop,','            stop,\n            connected: std::sync::atomic::AtomicBool::new(true),');s=s.replace('                    Ok(view) => *owner.view.write() = view,','''                    Ok(view) => {
                        owner.connected.store(true, std::sync::atomic::Ordering::Relaxed);
                        *owner.view.write() = view;
                    }''');s=s.replace('                    Err(error) => {','                    Err(error) => {\n                        owner.connected.store(false, std::sync::atomic::Ordering::Relaxed);',1).replace('    pub fn view(&self) -> ServerView {','''    pub fn is_connected(&self) -> bool {
        self.connected.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn view(&self) -> ServerView {''');p.write_text(s,encoding='utf-8')
p=Path('crates/gui/src/server.rs');s=p.read_text(encoding='utf-8').replace('''        let lost = matches!(&self.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Managed(c) if c.is_closed()));''','''        let lost = match &self.state {
            ServerState::Running {stack,..} => match &**stack {
                GuiStack::Managed(c) => c.is_closed(),
                GuiStack::Remote(c) => !c.is_connected(),
                _ => false,
            },
            _ => false,
        };''')
idx=s.index('    #[test]\n    fn save_during_start')
s=s[:idx]+'''    #[test]
    fn gui_owns_selected_controller_without_launching_a_child_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path,serde_json::json!({"schema_version":"1","gateway":{"instance_name":"selected","data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
        let mut server = EmbeddedServer::default();
        server.attach(path.clone());
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while !server.has_mcp() {
            server.pump();
            assert!(Instant::now()<deadline,"{:?}",server.last_error);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(matches!(&server.state,ServerState::Running{stack,..} if matches!(&**stack,GuiStack::Managed(_))));
        assert!(!server.is_running());
        let endpoint:serde_json::Value=serde_json::from_slice(&std::fs::read(format!("{}.mcp-endpoint.json",path.display())).unwrap()).unwrap();
        assert_eq!(endpoint["pid"],std::process::id());
        server.shutdown_blocking();
        assert!(!std::path::Path::new(&format!("{}.mcp-endpoint.json",path.display())).exists());
    }
'''+s[idx:];p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/tests/mcp_startup.rs');s=p.read_text(encoding='utf-8').replace('server_started_after_idle_mcp_host_uses_it_and_leaves_host_on_stop','server_started_after_configurator_uses_its_controller_and_leaves_it_on_stop');s+='''
#[tokio::test]
async fn different_configs_with_the_same_preferred_port_remain_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    for (path,name) in [(&first,"first"),(&second,"second")] {
        std::fs::write(path,json!({"schema_version":"1","gateway":{"instance_name":name},"mcp":{"port":port},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    }
    let a=Controller::new(&first).await.unwrap();
    let b=Controller::new(&second).await.unwrap();
    assert_ne!(a.url(),b.url());
    let ca=Client::connect(&first).await.unwrap();
    let cb=Client::connect(&second).await.unwrap();
    let sa=ca.call("server_status",json!({})).await.unwrap();
    let sb=cb.call("server_status",json!({})).await.unwrap();
    assert_ne!(sa["config_id"],sb["config_id"]);
    assert_eq!(sa["instance_name"],"first");
    assert_eq!(sb["instance_name"],"second");
    a.shutdown().await;
    b.shutdown().await;
}
''';p.write_text(s,encoding='utf-8')
