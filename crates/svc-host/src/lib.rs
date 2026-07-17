//! Service hosting (plan §7): the shared run-stack used by both the console
//! process and the Windows service, plus the systemd unit template and the
//! Windows SCM integration (install / uninstall / dispatch).
//!
//! - Linux: no wrapper needed — systemd runs the plain `run` command and
//!   captures stdout into the journal. [`systemd_unit`] prints the unit file
//!   (the .deb ships the same file in phase 8).
//! - Windows: [`windows`] registers with the SCM; the service entry runs
//!   [`run_stack`] with an SCM-driven shutdown signal instead of Ctrl+C.

pub mod stack;
#[cfg(windows)]
pub mod windows;

pub use stack::{run_stack, start_stack, RunningStack, StackExit};

/// The systemd unit for the server. `exe` and `config` must be absolute.
pub fn systemd_unit(exe: &std::path::Path, config: &std::path::Path) -> String {
    // Belt and braces for relative config paths (B7): the loader already
    // rebases data_dir / logging.dir / pki_dir onto the config's directory,
    // and the unit anchors the process CWD there too.
    let workdir = config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("/"));
    format!(
        r#"[Unit]
Description=OPC Modbus Server (OPC UA server with a built-in Modbus poller)
Documentation=https://example.invalid/opc-modbus-server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exe} run --config {config}
WorkingDirectory={workdir}
Restart=on-failure
RestartSec=5
# Serial ports (RTU): the service user must be in the dialout group.
SupplementaryGroups=dialout
# Hardening (relax if the config/data dirs live elsewhere):
NoNewPrivileges=true
ProtectSystem=full
PrivateTmp=true

[Install]
WantedBy=multi-user.target
"#,
        exe = exe.display(),
        config = config.display(),
        workdir = workdir.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn systemd_unit_contains_the_essentials() {
        let unit = systemd_unit(
            Path::new("/usr/bin/opc-modbus-server"),
            Path::new("/etc/opc-modbus-server/config.json"),
        );
        for needle in [
            "ExecStart=/usr/bin/opc-modbus-server run --config /etc/opc-modbus-server/config.json",
            // B7: relative config paths anchor to the config's directory.
            "WorkingDirectory=/etc/opc-modbus-server",
            "Restart=on-failure",
            "WantedBy=multi-user.target",
            "SupplementaryGroups=dialout",
            "After=network-online.target",
        ] {
            assert!(unit.contains(needle), "missing: {needle}\n---\n{unit}");
        }
    }
}
