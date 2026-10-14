use serde_json::json;
use svc_host::mcp::Controller;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_controller_owns_each_config_even_with_an_ephemeral_port() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","mcp":{"port":0},"opcua":{"enabled":false},"gateway":{"data_dir":dir.path().join("data")},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let first = Controller::new(&path).await.unwrap();
    let second = Controller::new(&path).await;
    assert!(matches!(second,Err(error) if error.contains("already owns")));
    first.shutdown().await;
    // The old Arc remains alive, but closing its runtime releases ownership.
    let next = Controller::new(&path).await.unwrap();
    next.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staged_mcp_settings_do_not_stop_or_mislabel_the_active_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    std::fs::write(&path,json!({"schema_version":"1","mcp":{"port":0},"opcua":{"enabled":false},"gateway":{"data_dir":dir.path().join("data")},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let owner = Controller::new(&path).await.unwrap();
    owner.start_server().await.unwrap();
    let original = owner.call("get_config", json!({})).await.unwrap();
    let staged=owner.call("patch_config",json!({"expected_revision":original["revision"],"apply":false,"patch":{"mcp":{"enabled":false}}})).await.unwrap();
    assert_eq!(staged["applied"], false);
    assert!(owner
        .call(
            "apply_config",
            json!({"expected_revision":staged["revision"]})
        )
        .await
        .is_err());
    assert!(owner
        .call("server_restart", json!({"confirm":true}))
        .await
        .is_err());
    assert!(owner.view().running);
    let current = owner.call("get_config", json!({})).await.unwrap();
    assert_eq!(current["applied_revision"], original["applied_revision"]);
    assert_eq!(current["active_config"]["mcp"]["port"], 0);
    assert_eq!(current["config"]["mcp"]["enabled"], false);
    owner.shutdown().await;
}
