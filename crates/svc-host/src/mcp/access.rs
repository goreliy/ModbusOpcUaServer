use super::{parse_file, read_file, validate};
use serde_json::{json, Value};
use std::{io::Write, path::Path};

fn outcome(result: Result<(), String>) -> Value {
    match result {
        Ok(()) => json!({"ok":true}),
        Err(e) => json!({"ok":false,"error":e}),
    }
}

/// Probe only application files/storage. Never starts pollers or OPC UA.
pub fn check_access(path: &Path, open_data_dir: Option<&str>) -> Result<Value, String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let cfg = validate(&parse_file(&read_file(&path)?)?, &path)?;
    let config = outcome(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map(|_| ())
            .map_err(|e| format!("{}: {e}", path.display())),
    );
    let directory = outcome((|| {
        let parent = path.parent().ok_or("config parent missing")?;
        let mut first = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        first
            .write_all(b"MCP access probe")
            .map_err(|e| e.to_string())?;
        let second = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        let target = second.into_temp_path();
        first.persist(&target).map_err(|e| e.to_string())?;
        // TempPath removes the probe after a successful atomic replacement.
        Ok::<(), String>(())
    })());
    let storage_path = std::path::PathBuf::from(&cfg.gateway.data_dir).join("tags.sled");
    let storage = if open_data_dir == Some(cfg.gateway.data_dir.as_str()) {
        json!({"ok":true,"already_open":true})
    } else {
        outcome((|| {
            std::fs::create_dir_all(storage_path.parent().unwrap()).map_err(|e| e.to_string())?;
            let store = tags_core::Persist::open(&storage_path).map_err(|e| {
                format!(
                    "{}: {e}; check permissions and another server's database lock",
                    storage_path.display()
                )
            })?;
            store.flush().map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        })())
    };
    Ok(
        json!({"ok":config["ok"]==true&&directory["ok"]==true&&storage["ok"]==true,"config_path":path,"config_file_writable":config,"directory_atomic_replace":directory,"storage_path":storage_path,"storage":storage,"hardware_accessed":false}),
    )
}
