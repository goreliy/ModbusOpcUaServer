use serde_json::json;
use svc_host::{mcp::check_access, stack::start_stack_detailed, StackExit};

#[tokio::test]
async fn access_and_start_errors_identify_the_storage_path_without_polling() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let data = dir.path().join("not-a-directory");
    std::fs::write(&data, b"occupied path").unwrap();
    std::fs::write(&path,json!({"schema_version":"1","gateway":{"data_dir":data},"opcua":{"enabled":false},"channels":[],"poll_groups":[]}).to_string()).unwrap();
    let access = check_access(&path, None).unwrap();
    assert_eq!(access["ok"], false);
    assert_eq!(access["hardware_accessed"], false);
    assert_eq!(access["storage"]["ok"], false);
    let error = match start_stack_detailed(gateway_config::load(&path).unwrap()).await {
        Err(e) => e,
        Ok(stack) => {
            stack.shutdown().await;
            panic!("storage must fail")
        }
    };
    assert_eq!(error.kind, StackExit::StorageError);
    assert!(error.message.contains("not-a-directory"));
    let mut cfg: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    cfg["gateway"]["data_dir"] = json!(dir.path().join("valid-data"));
    std::fs::write(&path, cfg.to_string()).unwrap();
    let before = std::fs::read(&path).unwrap();
    let access = check_access(&path, None).unwrap();
    assert_eq!(access["ok"], true);
    assert_eq!(before, std::fs::read(&path).unwrap());
}
