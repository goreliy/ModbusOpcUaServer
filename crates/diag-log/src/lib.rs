//! Logging initialization (plan §6).
//!
//! - Console layer is always on: interactive runs read it directly, and under
//!   systemd stdout lands in the journal — that IS the journald integration.
//! - `logging.dir` additionally enables a daily-rotated file
//!   (`<prefix>.YYYY-MM-DD`) behind a non-blocking writer. Required in
//!   practice for the Windows service, which has no console.
//! - The filter comes from `logging.level` (tracing env-filter syntax, e.g.
//!   "info,mb_poller=debug,modbus_traffic=debug"); the `RUST_LOG` environment
//!   variable overrides it. An unparseable filter falls back to "info" —
//!   a logging typo must not keep the plant server down.
//!
//! The returned [`LogGuards`] must stay alive for the process lifetime:
//! dropping it flushes and stops the background writer.

use gateway_config::schema::v1::LoggingConfig;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Keep alive until exit (non-blocking writer flush).
pub struct LogGuards {
    _file: Option<WorkerGuard>,
}

/// What `init` decided, for the caller to log/print afterwards.
pub struct LogSetup {
    pub guards: LogGuards,
    /// The effective filter string.
    pub filter: String,
    /// Set when `logging.level` failed to parse and "info" was used instead.
    pub filter_error: Option<String>,
    /// The log file path prefix, when file logging is enabled.
    pub file: Option<std::path::PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    #[error("cannot create log dir {dir}: {source}")]
    Dir {
        dir: String,
        source: std::io::Error,
    },
    #[error("logging already initialized")]
    AlreadyInit,
}

/// Install the global tracing subscriber per the config. Call once, early.
pub fn init(cfg: &LoggingConfig) -> Result<LogSetup, LogInitError> {
    // RUST_LOG (operator override) > config > "info".
    let filter_src = match std::env::var("RUST_LOG") {
        Ok(env) if !env.trim().is_empty() => env,
        _ => cfg.level.clone(),
    };
    let (env_filter, filter, filter_error) = match EnvFilter::try_new(&filter_src) {
        Ok(f) => (f, filter_src, None),
        Err(e) => (
            EnvFilter::new("info"),
            "info".to_string(),
            Some(format!("bad logging.level `{filter_src}`: {e}")),
        ),
    };

    let console = tracing_subscriber::fmt::layer().with_target(true);

    let (file_layer, file_guard, file_path) = match &cfg.dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).map_err(|source| LogInitError::Dir {
                dir: dir.clone(),
                source,
            })?;
            let appender = tracing_appender::rolling::daily(dir, &cfg.file_prefix);
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(writer);
            (
                Some(layer),
                Some(guard),
                Some(std::path::Path::new(dir).join(&cfg.file_prefix)),
            )
        }
        None => (None, None, None),
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console)
        .with(file_layer)
        .try_init()
        .map_err(|_| LogInitError::AlreadyInit)?;

    Ok(LogSetup {
        guards: LogGuards { _file: file_guard },
        filter,
        filter_error,
        file: file_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The global subscriber can be installed once per process, so a single
    // test exercises the whole surface.
    #[test]
    fn init_with_file_writes_and_bad_filter_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LoggingConfig {
            // An invalid LEVEL is what actually fails EnvFilter's lenient
            // parser (a stray word would just be treated as a target name).
            level: "foo=notalevel".into(),
            dir: Some(dir.path().to_string_lossy().to_string()),
            file_prefix: "test-log".into(),
        };
        std::env::remove_var("RUST_LOG");
        let setup = init(&cfg).expect("init");
        assert_eq!(setup.filter, "info", "bad filter falls back");
        assert!(setup.filter_error.is_some());
        assert!(setup.file.is_some());

        tracing::info!(probe = 42, "diag-log file smoke");
        drop(setup); // flush the non-blocking writer

        let mut found = false;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let p = entry.unwrap().path();
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            if text.contains("diag-log file smoke") {
                found = true;
            }
        }
        assert!(found, "the log line must land in the rolled file");

        // Second init fails cleanly.
        assert!(matches!(init(&cfg), Err(LogInitError::AlreadyInit)));
    }
}
