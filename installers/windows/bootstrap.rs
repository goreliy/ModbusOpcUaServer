use std::fs;
use std::path::Path;
use std::process::{self, Command};
use std::time::{SystemTime, UNIX_EPOCH};

static PAYLOAD: &[u8] = include_bytes!(env!("OMS_INSTALLER_PAYLOAD"));

fn quote_powershell(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

fn fail(message: &str) -> ! {
    eprintln!("OPC Modbus Server installer error: {message}");
    eprintln!("Press any key to close this window.");
    let _ = Command::new("cmd.exe")
        .args(["/d", "/c", "pause >nul"])
        .status();
    process::exit(1);
}

fn main() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let temp_dir = std::env::temp_dir().join(format!(
        "OPC-Modbus-Server-Installer-{}-{nonce}",
        process::id()
    ));
    let archive = temp_dir.join("payload.zip");
    let install_script = temp_dir.join("install.ps1");

    fs::create_dir_all(&temp_dir)
        .unwrap_or_else(|error| fail(&format!("cannot create temp directory: {error}")));
    fs::write(&archive, PAYLOAD)
        .unwrap_or_else(|error| fail(&format!("cannot write embedded payload: {error}")));

    let command = format!(
        "$ErrorActionPreference='Stop'; Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force; & '{}'",
        quote_powershell(&archive),
        quote_powershell(&temp_dir),
        quote_powershell(&install_script)
    );
    let result = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &command,
        ])
        .status();

    let cleanup_result = fs::remove_dir_all(&temp_dir);
    match result {
        Ok(status) if status.success() => {
            if let Err(error) = cleanup_result {
                eprintln!("Warning: cannot remove temporary files: {error}");
            }
        }
        Ok(status) => fail(&format!("installation exited with {status}")),
        Err(error) => fail(&format!("cannot start PowerShell: {error}")),
    }
}
