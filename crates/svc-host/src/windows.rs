//! Windows service integration (SCM).
//!
//! - [`install`] / [`uninstall`] manage the service entry (need an elevated
//!   shell).
//! - [`run_service`] is the `service run` entry: it must be started BY the
//!   SCM (`sc start` / services.msc); outside the SCM the dispatcher fails
//!   and we return a helpful error instead of hanging.
//!
//! The service main loads the config, initializes logging (file logs are
//! essential — a service has no console) and drives
//! [`crate::run_stack`] with the SCM Stop/Shutdown event as the shutdown
//! signal.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const SERVICE_NAME: &str = "opc-modbus-server";
const DISPLAY_NAME: &str = "OPC Modbus Server";
const DESCRIPTION: &str =
    "OPC UA server with a built-in Modbus poller and formula engine";

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("windows service: {0}")]
    Ws(#[from] windows_service::Error),
    #[error("{0}")]
    Other(String),
}

/// Create the SCM entry: auto-start, `service run --config <abs path>`.
pub fn install(exe: &Path, config: &Path) -> Result<(), ServiceError> {
    let config = std::path::absolute(config)
        .map_err(|e| ServiceError::Other(format!("cannot absolutize config path: {e}")))?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.to_path_buf(),
        launch_arguments: vec![
            OsString::from("service"),
            OsString::from("run"),
            OsString::from("--config"),
            OsString::from(config.as_os_str()),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager.create_service(&info, ServiceAccess::CHANGE_CONFIG)?;
    service.set_description(DESCRIPTION)?;
    Ok(())
}

/// Remove the SCM entry (stops it first when running).
pub fn uninstall() -> Result<(), ServiceError> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        // Give the SCM a moment; deletion is queued anyway.
        std::thread::sleep(Duration::from_millis(500));
    }
    service.delete()?;
    Ok(())
}

/// The config path is smuggled to the service main through an env var — the
/// `define_windows_service!` entry receives only SCM launch arguments, and
/// parsing them again here keeps one source of truth (main already did clap).
pub const CONFIG_ENV: &str = "OPC_MODBUS_SERVER_CONFIG";

/// Start the SCM dispatcher (blocks for the service lifetime). Fails fast
/// with a clear message when not started by the SCM.
pub fn run_service(config: PathBuf) -> Result<(), ServiceError> {
    std::env::set_var(CONFIG_ENV, &config);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        ServiceError::Other(format!(
            "SCM dispatcher failed ({e}). `service run` must be started by the \
             Windows Service Control Manager (sc start {SERVICE_NAME}); use plain \
             `run` for a console session"
        ))
    })
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = service_main_inner() {
        tracing::error!(error = %e, "service main failed");
    }
}

fn service_main_inner() -> Result<(), ServiceError> {
    let config = PathBuf::from(
        std::env::var_os(CONFIG_ENV)
            .ok_or_else(|| ServiceError::Other(format!("{CONFIG_ENV} not set")))?,
    );

    // Config + logging first: without file logs a service failure is silent.
    let cfg = match gateway_config::load(&config) {
        Ok(cfg) => cfg,
        Err(e) => {
            // No logger yet — the event is at least visible in the SCM state.
            eprintln!("configuration invalid: {e}");
            return Err(ServiceError::Other(format!("configuration invalid: {e}")));
        }
    };
    let _log_guards = diag_log::init(&cfg.logging).ok();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);

    let status_handle = service_control_handler::register(SERVICE_NAME, move |control| {
        match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(running.clone())?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| ServiceError::Other(format!("tokio runtime: {e}")))?;
    let exit = runtime.block_on(crate::run_stack(cfg, async move {
        let _ = shutdown_rx.await;
    }));

    let stopped = ServiceStatus {
        current_state: ServiceState::Stopped,
        exit_code: ServiceExitCode::Win32(exit.code() as u32),
        controls_accepted: ServiceControlAccept::empty(),
        ..running
    };
    status_handle.set_service_status(stopped)?;
    Ok(())
}
