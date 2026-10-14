from pathlib import Path
p=Path('crates/svc-host/src/mcp/client.rs');s=p.read_text(encoding='utf-8').replace('    pub fn url(&self) -> String {','    pub fn instance_id(&self) -> &str { &self.endpoint.instance }\n    pub fn url(&self) -> String {');p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/src/mcp/mod.rs');s=p.read_text(encoding='utf-8').replace('''    let mut attached_once = false;''','''    let mut attached_once = false;
    let mut running_intent = true;
    let mut previous_instance = None;''');s=s.replace('''            if current.call("server_status", json!({})).await.is_ok() {
                continue;
            }''','''            if let Ok(status)=current.call("server_status",json!({})).await {
                running_intent=status["running"]==true;
                continue;
            }''');s=s.replace('''                if let Err(error) = controller.start_server().await {
                    controller.shutdown().await;
                    return Err(error);
                }''','''                if running_intent {
                    if let Err(error)=controller.start_server().await {
                        controller.shutdown().await; return Err(error);
                    }
                }''',1);s=s.replace('''                        if !attached_once {
                            current.call("server_start", json!({})).await?;
                        }
                        attached_once = true;
                        client = Some(current);''','''                        let changed=previous_instance.as_deref()!=Some(current.instance_id());
                        if running_intent && (!attached_once || changed) {current.call("server_start",json!({})).await?;}
                        previous_instance=Some(current.instance_id().to_owned());
                        attached_once=true;client=Some(current);''');p.write_text(s,encoding='utf-8')
p=Path('crates/svc-host/tests/mcp_startup.rs');s=p.read_text(encoding='utf-8');s+='''
#[tokio::test]
async fn server_takes_over_when_configurator_owner_closes() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let gui=Controller::new(&path).await.unwrap();
    let initial_url=gui.url();
    let (stop,shutdown)=tokio::sync::oneshot::channel();
    let runner=tokio::spawn(run_managed(path.clone(),async{let _=shutdown.await;}));
    tokio::time::timeout(Duration::from_secs(3),async{while !gui.view().running{tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    gui.shutdown().await;
    let replacement=tokio::time::timeout(Duration::from_secs(5),async{loop{if let Ok(client)=Client::connect(&path).await{let status=client.call("server_status",json!({})).await.unwrap();if status["running"]==true{break client;}}tokio::time::sleep(Duration::from_millis(20)).await;}}).await.unwrap();
    assert_ne!(Some(replacement.url()),initial_url);
    assert!(!runner.is_finished());
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5),runner).await.unwrap().unwrap().unwrap();
    assert!(Client::connect(&path).await.is_err());
}

#[tokio::test]
async fn explicit_controller_shutdown_is_not_automatically_undone_by_attached_server() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let gui=Controller::new(&path).await.unwrap();
    let runner=tokio::spawn(run_managed(path.clone(),std::future::pending()));
    tokio::time::timeout(Duration::from_secs(3),async{while !gui.view().running{tokio::time::sleep(Duration::from_millis(10)).await;}}).await.unwrap();
    gui.call("shutdown_controller",json!({"confirm":true})).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5),runner).await.unwrap().unwrap().unwrap();
    assert!(svc_host::mcp::shutdown_was_requested(&path));
    gui.shutdown().await;
    assert!(Client::connect(&path).await.is_err());
}
''';p.write_text(s,encoding='utf-8')
