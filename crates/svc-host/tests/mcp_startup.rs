use serde_json::json;
use std::time::Duration;
use svc_host::mcp::{run_managed, Client, Controller};

#[tokio::test]
async fn server_started_after_configurator_uses_its_controller_and_leaves_it_on_stop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path, json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let owner = Controller::new(&path).await.unwrap();
    let original_url = owner.url();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let runner = tokio::spawn(run_managed(path.clone(), async {
        let _ = shutdown.await;
    }));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !owner.view().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(owner.url(), original_url);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!owner.view().running);
    assert!(Client::connect(&path).await.is_ok());
    owner.shutdown().await;
}

#[tokio::test]
async fn different_configs_with_the_same_preferred_port_remain_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    for (path, name) in [(&first, "first"), (&second, "second")] {
        std::fs::write(path,json!({"schema_version":"1","gateway":{"instance_name":name},"mcp":{"port":port},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    }
    let a = Controller::new(&first).await.unwrap();
    let b = Controller::new(&second).await.unwrap();
    assert_ne!(a.url(), b.url());
    let ca = Client::connect(&first).await.unwrap();
    let cb = Client::connect(&second).await.unwrap();
    let sa = ca.call("server_status", json!({})).await.unwrap();
    let sb = cb.call("server_status", json!({})).await.unwrap();
    assert_ne!(sa["config_id"], sb["config_id"]);
    assert_eq!(sa["instance_name"], "first");
    assert_eq!(sb["instance_name"], "second");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn server_takes_over_when_configurator_owner_closes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let gui = Controller::new(&path).await.unwrap();
    let initial_url = gui.url();
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let runner = tokio::spawn(run_managed(path.clone(), async {
        let _ = shutdown.await;
    }));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !gui.view().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    gui.shutdown().await;
    let replacement = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(client) = Client::connect(&path).await {
                let status = client.call("server_status", json!({})).await.unwrap();
                if status["running"] == true {
                    break client;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(Some(replacement.url()), initial_url);
    assert!(!runner.is_finished());
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(Client::connect(&path).await.is_err());
}

#[tokio::test]
async fn explicit_controller_shutdown_is_not_automatically_undone_by_attached_server() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","gateway":{"data_dir":dir.path().join("data")},"mcp":{"port":0},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let gui = Controller::new(&path).await.unwrap();
    let runner = tokio::spawn(run_managed(path.clone(), std::future::pending()));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !gui.view().running {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    gui.call("shutdown_controller", json!({"confirm":true}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(svc_host::mcp::shutdown_was_requested(&path));
    gui.shutdown().await;
    assert!(Client::connect(&path).await.is_err());
}
