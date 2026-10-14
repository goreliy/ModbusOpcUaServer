use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn server_executable() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if exe
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .starts_with("opc-modbus-config")
    {
        Ok(exe.with_file_name(if cfg!(windows) {
            "opc-modbus-server.exe"
        } else {
            "opc-modbus-server"
        }))
    } else {
        Ok(exe)
    }
}
pub fn default_config_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let nearby = exe.with_file_name("config.json");
        if nearby.is_file() {
            return nearby;
        }
    }
    if let Some(dir) = std::env::var_os("ProgramData") {
        let installed = PathBuf::from(dir).join("OPC Modbus Server/config.json");
        if installed.is_file() {
            return installed;
        }
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    base.join("OPC Modbus Server/config.json")
}
fn display(path: &Path) -> String {
    path.to_string_lossy()
        .strip_prefix(r"\\?\")
        .unwrap_or(&path.to_string_lossy())
        .to_owned()
}
pub fn config_id(path: &Path) -> String {
    let text = display(path);
    let text = if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    };
    super::fingerprint(text.as_bytes())[..12].to_owned()
}
pub fn connection_info(path: &Path) -> Result<Value, String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let executable = server_executable()?;
    let exe = display(&executable);
    let config = display(&path);
    let value = super::parse_file(&super::read_file(&path)?)?;
    let instance_name = value
        .pointer("/gateway/instance_name")
        .and_then(Value::as_str)
        .unwrap_or("OPC Modbus");
    let config_id = config_id(&path);
    let label: String = instance_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    let server_name = format!("opc_modbus_{label}_{config_id}");
    let args = json!(["mcp", "--config", config]);
    let command = serde_json::to_string(&exe).unwrap();
    let toml = format!(
        "[mcp_servers.{server_name}]\nenabled = true\ncommand = {command}\nargs = {args}\n"
    );
    let claude =
        json!({"mcpServers":{(server_name.clone()):{"type":"stdio","command":exe,"args":args}}});
    let cfg = super::validate(&super::parse_file(&super::read_file(&path)?)?, &path)?;
    Ok(
        json!({"config_id":config_id,"instance_name":instance_name,"server_name":server_name,"executable":exe,"executable_directory":executable.parent().map(display),"working_directory":std::env::current_dir().ok().map(|p|display(&p)),"config_path":config,"data_directory":cfg.gateway.data_dir,"logging_directory":cfg.logging.dir,"pki_directory":cfg.opcua.pki_dir,"codex_toml":toml,"claude_json":serde_json::to_string_pretty(&claude).unwrap(),"stdio_command":format!("\"{exe}\" mcp --config \"{config}\"")}),
    )
}
