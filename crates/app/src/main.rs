//! `opc-modbus-server` — the product binary: an OPC UA server whose data
//! source is the built-in Modbus poller with the formula engine in between.
//!
//!   opc-modbus-server run --config config.json          # console (Ctrl+C)
//!   opc-modbus-server validate --config config.json
//!   opc-modbus-server hash-password                     # stdin -> argon2id
//!   opc-modbus-server systemd-unit --config /etc/...    # print the unit file
//!   opc-modbus-server service install --config c.json   # Windows, elevated
//!   opc-modbus-server service uninstall                 # Windows, elevated
//!   opc-modbus-server service run --config c.json       # SCM entry only

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "opc-modbus-server",
    version,
    about = "OPC UA server with a built-in Modbus poller and formula engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a configuration file and print what it resolves to.
    Validate {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Run in the console (poller + tag engine + OPC UA), Ctrl+C to stop.
    Run {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Hash a password (argon2id) for `opcua.users[].password_hash`.
    /// Reads the password from stdin (avoids the shell history).
    HashPassword,
    /// Print a systemd unit file for this executable (Linux).
    SystemdUnit {
        /// Absolute config path to embed in ExecStart.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Windows service management.
    #[command(subcommand)]
    Service(ServiceCmd),
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Register the Windows service (run from an elevated shell).
    Install {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Remove the Windows service (run from an elevated shell).
    Uninstall,
    /// Service entry point — started by the SCM, not by hand.
    Run {
        #[arg(short, long)]
        config: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Validate { config } => validate(&config),
        Command::Run { config } => run_console(&config),
        Command::HashPassword => hash_password(),
        Command::SystemdUnit { config } => systemd_unit(&config),
        Command::Service(cmd) => service(cmd),
    };
    std::process::exit(code);
}

fn validate(path: &PathBuf) -> i32 {
    match gateway_config::load(path) {
        Ok(cfg) => {
            println!(
                "OK: {} channel(s), {} device(s), {} tag(s)",
                cfg.channels.len(),
                cfg.channels.iter().map(|c| c.devices.len()).sum::<usize>(),
                cfg.tag_count(),
            );
            for w in &cfg.warnings {
                println!("warning: {w}");
            }
            0
        }
        Err(e) => {
            eprintln!("configuration invalid:\n{e}");
            2
        }
    }
}

/// Console mode: config -> logging -> multi-thread runtime -> stack until Ctrl+C.
fn run_console(path: &PathBuf) -> i32 {
    let cfg = match gateway_config::load(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("configuration invalid:\n{e}");
            return 2;
        }
    };
    let log_setup = match diag_log::init(&cfg.logging) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("logging init failed: {e}");
            return 3;
        }
    };
    if let Some(err) = &log_setup.filter_error {
        tracing::warn!("{err}; using `info`");
    }
    if let Some(file) = &log_setup.file {
        tracing::info!(file = %file.display(), "file logging enabled (daily rotation)");
    }
    if let Some(lim) = svc_host::stack::TRIAL_LIMIT {
        tracing::warn!(
            "ПРОБНАЯ СБОРКА: непрерывная работа ограничена {} ч",
            lim.as_secs() / 3600
        );
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "tokio runtime");
            return 3;
        }
    };
    let exit = runtime.block_on(svc_host::run_stack(cfg, async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for Ctrl+C");
            std::future::pending::<()>().await;
        }
        tracing::info!("Ctrl+C received");
    }));
    let _ = log_setup; // keep the writer alive to the very end
    exit.code()
}

fn hash_password() -> i32 {
    use std::io::Read;
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("could not read the password from stdin");
        return 2;
    }
    let password = input.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        eprintln!("empty password (pipe it in: echo secret | opc-modbus-server hash-password)");
        return 2;
    }
    match opcua_gateway::hash_password(password) {
        Ok(phc) => {
            println!("{phc}");
            0
        }
        Err(e) => {
            eprintln!("hashing failed: {e}");
            1
        }
    }
}

fn systemd_unit(config: &std::path::Path) -> i32 {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/usr/bin/opc-modbus-server"));
    print!("{}", svc_host::systemd_unit(&exe, config));
    0
}

#[cfg(windows)]
fn service(cmd: ServiceCmd) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot determine executable path: {e}");
            return 1;
        }
    };
    match cmd {
        ServiceCmd::Install { config } => {
            // Refuse to register a broken config outright.
            if gateway_config::load(&config).is_err() {
                eprintln!("configuration invalid — fix it before installing the service");
                return 2;
            }
            match svc_host::windows::install(&exe, &config) {
                Ok(()) => {
                    println!(
                        "service `{}` installed (auto-start). Start it with: sc start {}",
                        svc_host::windows::SERVICE_NAME,
                        svc_host::windows::SERVICE_NAME
                    );
                    0
                }
                Err(e) => {
                    eprintln!("install failed (elevated shell required): {e}");
                    1
                }
            }
        }
        ServiceCmd::Uninstall => match svc_host::windows::uninstall() {
            Ok(()) => {
                println!("service removed");
                0
            }
            Err(e) => {
                eprintln!("uninstall failed (elevated shell required): {e}");
                1
            }
        },
        ServiceCmd::Run { config } => match svc_host::windows::run_service(config) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
    }
}

#[cfg(not(windows))]
fn service(_cmd: ServiceCmd) -> i32 {
    eprintln!(
        "`service` commands manage the Windows SCM; on Linux use systemd \
         (see `opc-modbus-server systemd-unit --config ...`)"
    );
    2
}
