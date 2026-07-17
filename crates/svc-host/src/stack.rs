//! The product stack as one function: poller -> tag engine -> OPC UA server,
//! brought up in order and torn down in reverse when `shutdown` resolves.
//! Shared by the console entry (Ctrl+C) and the Windows service (SCM Stop).
//!
//! F5 groundwork: the stack is also available as an observable object —
//! [`start_stack`] returns a [`RunningStack`] whose live tag store, channel
//! metrics and OPC UA handle a GUI can poll while the stack runs.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gateway_config::ResolvedConfig;

/// Лимит непрерывной работы для ПРОБНОЙ сборки (feature `trial`): по его
/// истечении стек сам корректно останавливается. В обычной сборке — `None`.
#[cfg(feature = "trial")]
pub const TRIAL_LIMIT: Option<Duration> = Some(Duration::from_secs(3 * 60 * 60));
#[cfg(not(feature = "trial"))]
pub const TRIAL_LIMIT: Option<Duration> = None;

/// Exit disposition of one stack run, mapped to a process exit code by main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackExit {
    /// Shutdown signal received, everything stopped cleanly.
    Clean,
    /// Storage (sled) could not be opened.
    StorageError,
    /// Tag engine failed to start (e.g. bad formula).
    EngineError,
    /// OPC UA server failed to start.
    OpcUaError,
}

impl StackExit {
    pub fn code(self) -> i32 {
        match self {
            StackExit::Clean => 0,
            StackExit::StorageError => 3,
            StackExit::EngineError => 4,
            StackExit::OpcUaError => 5,
        }
    }
}

/// A live stack (storage -> poller -> engine -> OPC UA), observable while it
/// runs. Created by [`start_stack`]; must be torn down with
/// [`RunningStack::shutdown`] (dropping it stops the tasks less gracefully —
/// see `PollerHandle`'s drop semantics).
pub struct RunningStack {
    poller: mb_poller::PollerHandle,
    engine: tags_core::EngineHandle,
    typed: Arc<tags_core::TypedStore>,
    opcua: Option<opcua_gateway::OpcUaHandle>,
    /// Channel display name + counters, in config order (enabled channels).
    channel_metrics: Vec<(String, Arc<mb_poller::ChannelMetrics>)>,
    /// When the stack came up (trial-limit countdown, uptime).
    started: Instant,
}

impl RunningStack {
    /// Time left before the trial limit stops the server, if this is a trial
    /// build. `None` in a full build; `Some(0)` once expired.
    pub fn trial_remaining(&self) -> Option<Duration> {
        TRIAL_LIMIT.map(|lim| lim.saturating_sub(self.started.elapsed()))
    }

    /// True when the trial limit has been reached (trial build only).
    pub fn trial_expired(&self) -> bool {
        self.trial_remaining().is_some_and(|r| r.is_zero())
    }

    /// Live typed tag values (for UIs).
    pub fn typed(&self) -> Arc<tags_core::TypedStore> {
        Arc::clone(&self.typed)
    }

    /// Channel display name + live counters, one entry per enabled channel,
    /// in config order.
    pub fn channel_metrics(&self) -> Vec<(String, Arc<mb_poller::ChannelMetrics>)> {
        self.channel_metrics.clone()
    }

    /// The OPC UA server handle (`session_count()` / `recent_authentications()`),
    /// if `opcua.enabled`.
    pub fn opcua(&self) -> Option<&opcua_gateway::OpcUaHandle> {
        self.opcua.as_ref()
    }

    /// Client-facing endpoint URL (advertised host), if OPC UA is running.
    pub fn endpoint_url(&self) -> Option<String> {
        self.opcua.as_ref().map(|h| h.endpoint_url())
    }

    /// Tear down in reverse bring-up order: opcua -> engine -> poller.
    pub async fn shutdown(self) {
        tracing::info!("shutting down (opcua -> engine -> poller)");
        if let Some(h) = self.opcua {
            h.shutdown().await;
        }
        self.engine.shutdown().await;
        self.poller.shutdown().await;
        tracing::info!("clean shutdown complete");
    }
}

/// Bring up the full stack (storage -> poller -> engine -> OPC UA) and hand
/// back the observable [`RunningStack`]. On a start failure the already
/// started layers are torn down and the matching [`StackExit`] variant is
/// returned. Must be called inside a multi-thread tokio runtime (the OPC UA
/// write path uses `block_in_place`).
pub async fn start_stack(cfg: ResolvedConfig) -> Result<RunningStack, StackExit> {
    for w in &cfg.warnings {
        tracing::warn!("config: {w}");
    }

    // ---- storage (retentive tags / history) ----
    let data_dir = PathBuf::from(&cfg.gateway.data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        tracing::error!(dir = %data_dir.display(), error = %e, "cannot create data dir");
        return Err(StackExit::StorageError);
    }
    let persist = match tags_core::Persist::open(data_dir.join("tags.sled")) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::error!(error = %e, "cannot open persistent tag store");
            return Err(StackExit::StorageError);
        }
    };

    // ---- phase-1 stack: poller + raw cache ----
    let (poller, cache) = mb_poller::Poller::spawn_with_cache(&cfg);
    tracing::info!(
        channels = cfg.channels.iter().filter(|c| c.enabled).count(),
        tags = cfg.tag_count(),
        "modbus poller started"
    );
    let channel_metrics: Vec<(String, Arc<mb_poller::ChannelMetrics>)> = cfg
        .channels
        .iter()
        .filter(|c| c.enabled)
        .filter_map(|c| Some((c.name.clone(), poller.metrics(c.id)?)))
        .collect();

    // ---- phase-2 stack: tag engine ----
    let engine = match tags_core::TagEngine::new(
        &cfg,
        Arc::clone(&cache) as Arc<dyn mb_poller::CacheReader>,
        persist,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "tag engine failed to start");
            poller.shutdown().await;
            return Err(StackExit::EngineError);
        }
    };
    let (engine_handle, typed) = engine.spawn();
    tracing::info!("tag engine started");

    // ---- phase-3/4/5 stack: the OPC UA server ----
    let opcua = if cfg.opcua.enabled {
        match opcua_gateway::spawn(
            &cfg,
            Arc::clone(&typed) as Arc<dyn tags_core::TypedReader>,
            poller.all_writers(),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::error!(error = %e, "OPC UA server failed to start");
                engine_handle.shutdown().await;
                poller.shutdown().await;
                return Err(StackExit::OpcUaError);
            }
        }
    } else {
        tracing::warn!("opcua.enabled = false: running headless (poller + engine only)");
        None
    };

    tracing::info!("opc-modbus-server up");
    if let Some(lim) = TRIAL_LIMIT {
        tracing::warn!(
            "ПРОБНАЯ СБОРКА: сервер остановится после {} ч непрерывной работы",
            lim.as_secs() / 3600
        );
    }
    Ok(RunningStack {
        poller,
        engine: engine_handle,
        typed,
        opcua,
        channel_metrics,
        started: Instant::now(),
    })
}

/// Run the full stack until `shutdown` resolves. Must be called inside a
/// multi-thread tokio runtime (the OPC UA write path uses `block_in_place`).
pub async fn run_stack(cfg: ResolvedConfig, shutdown: impl Future<Output = ()>) -> StackExit {
    let stack = match start_stack(cfg).await {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    // Trial build: whichever comes first — the external shutdown signal or the
    // trial time limit — stops the stack cleanly.
    match TRIAL_LIMIT {
        Some(lim) => {
            tokio::select! {
                _ = shutdown => {}
                _ = tokio::time::sleep(lim) => {
                    tracing::warn!(
                        "ПРОБНАЯ СБОРКА: истёк лимит {} ч непрерывной работы — останов",
                        lim.as_secs() / 3600
                    );
                }
            }
        }
        None => shutdown.await,
    }
    stack.shutdown().await;
    StackExit::Clean
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_cfg(dir: &std::path::Path) -> ResolvedConfig {
        gateway_config::load_str(&format!(
            r#"{{
            "schema_version": "1",
            "gateway": {{ "data_dir": {data_dir:?} }},
            "opcua": {{ "enabled": false }},
            "poll_groups": [],
            "channels": []
        }}"#,
            data_dir = dir.join("data"),
        ))
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stack_runs_and_stops_on_signal() {
        // Zero channels, OPC UA disabled: the minimal stack still has to come
        // up and shut down cleanly on signal.
        let dir = tempfile::tempdir().unwrap();
        let cfg = minimal_cfg(dir.path());

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let run = tokio::spawn(run_stack(cfg, async move {
            let _ = rx.await;
        }));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        tx.send(()).unwrap();
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("stack must stop on signal")
            .unwrap();
        assert_eq!(exit, StackExit::Clean);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_stack_is_observable_and_shuts_down() {
        // F5 groundwork: the object form exposes the live tag store and the
        // channel metrics, and tears down cleanly.
        let dir = tempfile::tempdir().unwrap();
        let cfg = minimal_cfg(dir.path());

        let stack = start_stack(cfg).await.expect("minimal stack must start");

        // typed(): accessible; zero tags configured -> empty store.
        let typed = stack.typed();
        assert!(typed.is_empty(), "no tags configured");

        // channel_metrics(): zero enabled channels -> empty, but callable.
        assert!(stack.channel_metrics().is_empty());

        // opcua disabled -> no handle, no endpoint URL.
        assert!(stack.opcua().is_none());
        assert_eq!(stack.endpoint_url(), None);

        tokio::time::timeout(std::time::Duration::from_secs(5), stack.shutdown())
            .await
            .expect("shutdown must complete");
    }
}
