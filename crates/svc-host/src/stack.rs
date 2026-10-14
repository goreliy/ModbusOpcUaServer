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

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StackStartError {
    pub kind: StackExit,
    pub message: String,
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
    poller: Option<mb_poller::PollerHandle>,
    engine: Option<tags_core::EngineHandle>,
    typed: Arc<tags_core::TypedStore>,
    opcua: Option<opcua_gateway::OpcUaHandle>,
    /// Channel display name + counters, in config order (enabled channels).
    channel_metrics: Vec<(String, Arc<mb_poller::ChannelMetrics>)>,
    /// When the stack came up (trial-limit countdown, uptime).
    started: Instant,
    cfg: Arc<ResolvedConfig>,
    cache: Arc<mb_poller::TagCache>,
    persist: tags_core::Persist,
    revision: u64,
}

impl RunningStack {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn config(&self) -> &ResolvedConfig {
        &self.cfg
    }
    pub fn calculated(&self) -> Arc<tags_core::TypedStore> {
        self.engine
            .as_ref()
            .map(|e| e.calculated())
            .unwrap_or_else(|| Arc::clone(&self.typed))
    }
    pub fn can_reload(&self, cfg: &ResolvedConfig) -> Result<(), String> {
        if self.opcua.as_ref().is_some_and(|o| !o.is_running()) {
            return Err("OPC UA server is not running".into());
        }
        if serde_json::to_value(&cfg.opcua).map_err(|e| e.to_string())?
            != serde_json::to_value(&self.cfg.opcua).map_err(|e| e.to_string())?
            || serde_json::to_value(&cfg.logging).map_err(|e| e.to_string())?
                != serde_json::to_value(&self.cfg.logging).map_err(|e| e.to_string())?
            || cfg.gateway.data_dir != self.cfg.gateway.data_dir
        {
            return Err("endpoint/security, logging and data_dir changes need server_stop, config edit and server_start".into());
        }
        Ok(())
    }
    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
    pub fn writer(
        &self,
        channel: mb_types::ChannelId,
    ) -> Option<tokio::sync::mpsc::Sender<mb_poller::WriteCommand>> {
        self.poller.as_ref()?.writer(channel)
    }
    pub fn history(&self, name: &str) -> Result<Vec<tags_core::StoredValue>, String> {
        self.persist.load_history(name).map_err(|e| e.to_string())
    }
    pub fn write_tag(&self, name: &str, value: serde_json::Value) -> Result<String, String> {
        if self.cfg.channels.iter().flat_map(|c| &c.devices).any(|d| {
            d.unit_id == 0
                && d.registers
                    .iter()
                    .any(|r| self.cfg.tag_name(r.tag) == Some(name))
        }) {
            return Err("broadcast writes are not permitted through MCP".into());
        }
        let writers = self.poller.as_ref().ok_or("poller stopped")?.all_writers();
        let plans =
            opcua_gateway::build_write_plans(&self.cfg, &writers).map_err(|e| e.to_string())?;
        let plan = plans
            .get(name)
            .ok_or("tag unknown, disabled or not writable")?;
        let status = plan.execute_json(value)?;
        if status.is_good() {
            Ok(status.to_string())
        } else {
            Err(status.to_string())
        }
    }

    /// Prepare the complete new generation before disturbing the old one.
    /// Formula-only edits keep the poller and transport connections running.
    /// Poll-plan edits drain old workers before spawning replacements; the
    /// process, OPC UA listener and sessions remain alive.
    pub async fn reload(&mut self, cfg: ResolvedConfig) -> Result<(), String> {
        if format!("{cfg:?}") == format!("{:?}", self.cfg) {
            return Ok(());
        }
        if self.opcua.as_ref().is_some_and(|o| !o.is_running()) {
            return Err(
                "OPC UA server/value pump is not running; reload cannot be committed".into(),
            );
        }
        if serde_json::to_value(&cfg.opcua).map_err(|e| e.to_string())?
            != serde_json::to_value(&self.cfg.opcua).map_err(|e| e.to_string())?
            || serde_json::to_value(&cfg.logging).map_err(|e| e.to_string())?
                != serde_json::to_value(&self.cfg.logging).map_err(|e| e.to_string())?
            || cfg.gateway.data_dir != self.cfg.gateway.data_dir
        {
            return Err("hot reload: OPC UA endpoint/security, logging and data_dir are startup settings; tag and poll configurations can be changed live".into());
        }
        let old_regs = register_signatures(&self.cfg);
        let new_regs = register_signatures(&cfg);
        let polling_changed = cfg.tag_names != self.cfg.tag_names
            || new_regs
                .iter()
                .any(|(name, (source, _))| old_regs.get(name).is_none_or(|(old, _)| old != source))
            || format!("{:?}", mb_poller::build_all(&cfg))
                != format!("{:?}", mb_poller::build_all(&self.cfg));
        let cache = if polling_changed {
            Arc::new(mb_poller::TagCache::new(&cfg))
        } else {
            Arc::clone(&self.cache)
        };
        // Compilation, graph validation and persistence reads all happen here,
        // before writes/polling are paused. Bad edits leave the old generation.
        let mut engine =
            tags_core::TagEngine::new(&cfg, Arc::clone(&cache) as _, Some(self.persist.clone()))
                .map_err(|e| e.to_string())?;
        let typed = engine.store();
        // A reload must not restore a former source's retentive value under
        // an edited acquisition/transform recipe. Seed only compatible live
        // values below; boot-time persistence restoration is unchanged.
        for i in 0..cfg.tag_count() {
            typed.clear(mb_types::TagId(i as u32));
        }
        use mb_poller::CacheReader;
        use tags_core::TypedReader;
        for (name, (source, transform)) in &new_regs {
            if let Some((old_source, old_transform)) = old_regs.get(name) {
                if source == old_source {
                    let old = self.cache.resolve(name).expect("resolved old tag");
                    let next = cache.resolve(name).expect("resolved new tag");
                    if polling_changed {
                        if let Some(snap) = self.cache.snapshot(old) {
                            cache.seed(next, snap);
                        }
                    }
                    if transform == old_transform {
                        if let Some(snap) = self.typed.snapshot(old) {
                            typed.seed(next, snap);
                        }
                    }
                }
            }
        }
        // Validate inverse write plans with inert senders as well. No device
        // connection or command is made during preflight.
        let writers = cfg
            .channels
            .iter()
            .filter(|c| c.enabled)
            .map(|c| {
                let (tx, _rx) = tokio::sync::mpsc::channel(1);
                (c.id, tx)
            })
            .collect();
        opcua_gateway::build_write_plans(&cfg, &writers).map_err(|e| e.to_string())?;
        if let Some(opcua) = &self.opcua {
            opcua.pause_writes();
        }
        if let Some(handle) = self.engine.take() {
            handle.shutdown().await;
        }
        for (name, recipe) in &old_regs {
            if new_regs.get(name) != Some(recipe) {
                if let Err(error) = self.persist.forget_retentive(name) {
                    tracing::error!(tag = name, %error, "cannot invalidate incompatible retentive value");
                }
            }
        }
        if polling_changed {
            if let Some(handle) = self.poller.take() {
                handle.shutdown().await;
            }
            // Capture the final raw snapshots after all old writers drained.
            for (name, (source, _)) in &new_regs {
                if old_regs.get(name).is_some_and(|(old, _)| old == source) {
                    if let (Some(old), Some(next)) = (self.cache.resolve(name), cache.resolve(name))
                    {
                        if let Some(snap) = self.cache.snapshot(old) {
                            cache.seed(next, snap);
                        }
                    }
                }
            }
            self.poller = Some(mb_poller::Poller::spawn(&cfg, Arc::clone(&cache) as _));
        }
        // Copy the final published values after the old engine has drained,
        // not values sampled while preflight was still running.
        for (name, recipe) in &new_regs {
            if old_regs.get(name) == Some(recipe) {
                if let (Some(old), Some(next)) = (self.typed.resolve(name), typed.resolve(name)) {
                    if let Some(snap) = self.typed.snapshot(old) {
                        typed.seed(next, snap);
                    }
                }
            }
        }
        engine.refresh();
        let (engine_handle, typed) = engine.spawn();
        self.engine = Some(engine_handle);
        let cfg = Arc::new(cfg);
        self.channel_metrics = collect_metrics(&cfg, self.poller.as_ref().expect("poller running"));
        self.typed = typed;
        self.cache = cache;
        self.cfg = cfg;
        self.revision += 1;
        if let Some(opcua) = &self.opcua {
            opcua
                .reconfigure(
                    Arc::clone(&self.cfg),
                    Arc::clone(&self.typed) as _,
                    self.poller.as_ref().expect("poller running").all_writers(),
                )
                .await
                .map_err(|e| {
                    format!(
                        "generation {} active, but OPC UA pump failed during commit: {e}",
                        self.revision
                    )
                })?;
        }
        tracing::info!(
            revision = self.revision,
            polling_changed,
            "configuration reloaded without process restart"
        );
        Ok(())
    }
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
        if let Some(h) = self.engine {
            h.shutdown().await;
        }
        if let Some(h) = self.poller {
            h.shutdown().await;
        }
        tracing::info!("clean shutdown complete");
    }
}

/// Bring up the full stack (storage -> poller -> engine -> OPC UA) and hand
/// back the observable [`RunningStack`]. On a start failure the already
/// started layers are torn down and the matching [`StackExit`] variant is
/// returned. Must be called inside a multi-thread tokio runtime (the OPC UA
/// write path uses `block_in_place`).
pub async fn start_stack(cfg: ResolvedConfig) -> Result<RunningStack, StackExit> {
    start_stack_detailed(cfg).await.map_err(|e| e.kind)
}

pub async fn start_stack_detailed(cfg: ResolvedConfig) -> Result<RunningStack, StackStartError> {
    for w in &cfg.warnings {
        tracing::warn!("config: {w}");
    }

    // ---- storage (retentive tags / history) ----
    let data_dir = PathBuf::from(&cfg.gateway.data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        tracing::error!(dir = %data_dir.display(), error = %e, "cannot create data dir");
        return Err(StackStartError {
            kind: StackExit::StorageError,
            message: format!("cannot create data directory {}: {e}", data_dir.display()),
        });
    }
    let persist = match tags_core::Persist::open(data_dir.join("tags.sled")) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "cannot open persistent tag store");
            return Err(StackStartError {kind:StackExit::StorageError,message:format!("cannot open tag storage {}: {e}. Check write permissions and whether another server owns this database",data_dir.join("tags.sled").display())});
        }
    };

    // ---- phase-1 stack: poller + raw cache ----
    let cache = Arc::new(mb_poller::TagCache::new(&cfg));

    // ---- phase-2 stack: tag engine ----
    let engine = match tags_core::TagEngine::new(
        &cfg,
        Arc::clone(&cache) as Arc<dyn mb_poller::CacheReader>,
        Some(persist.clone()),
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "tag engine failed to start");
            return Err(StackStartError {
                kind: StackExit::EngineError,
                message: format!("tag engine failed: {e}"),
            });
        }
    };
    let poller = mb_poller::Poller::spawn(&cfg, Arc::clone(&cache) as _);
    let channel_metrics = collect_metrics(&cfg, &poller);
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
                return Err(StackStartError {
                    kind: StackExit::OpcUaError,
                    message: format!("OPC UA startup failed: {e}"),
                });
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
        poller: Some(poller),
        engine: Some(engine_handle),
        typed,
        opcua,
        channel_metrics,
        started: Instant::now(),
        cfg: Arc::new(cfg),
        cache,
        persist,
        revision: 1,
    })
}

fn collect_metrics(
    cfg: &ResolvedConfig,
    poller: &mb_poller::PollerHandle,
) -> Vec<(String, Arc<mb_poller::ChannelMetrics>)> {
    cfg.channels
        .iter()
        .filter(|c| c.enabled)
        .filter_map(|c| Some((c.name.clone(), poller.metrics(c.id)?)))
        .collect()
}

/// Stable tag identity plus acquisition recipe. Dense IDs may change on edit.
fn register_signatures(
    cfg: &ResolvedConfig,
) -> std::collections::HashMap<String, (String, String)> {
    let mut out = std::collections::HashMap::new();
    for ch in &cfg.channels {
        for dev in &ch.devices {
            for reg in &dev.registers {
                let source = format!(
                    "{:?}/{}/{}/{}/{:?}/{}/{:?}/{:?}/{:?}/{:?}/{:?}/{:?}/{:?}/{:?}",
                    ch.transport,
                    ch.enabled,
                    dev.enabled,
                    dev.unit_id,
                    reg.function,
                    reg.address,
                    reg.data_type,
                    reg.word_order,
                    reg.byte_order,
                    reg.length,
                    reg.bit,
                    reg.custom_request,
                    reg.custom_response_len,
                    ch.name
                );
                let transform = format!(
                    "{:?}/{}/{}/{:?}",
                    reg.formula, reg.scale, reg.offset, reg.data_type
                );
                out.insert(
                    cfg.tag_names[reg.tag.0 as usize].clone(),
                    (source, transform),
                );
            }
        }
    }
    out
}

/// File-driven hot reload for console/systemd/SCM. Content comparison (rather
/// than mtime) handles atomic replacement, coarse timestamps and equal sizes.
/// Two identical reads debounce partially written files. Invalid candidates
/// are logged once and never stop the active generation.
pub async fn run_stack_watched(
    cfg: ResolvedConfig,
    path: PathBuf,
    shutdown: impl Future<Output = ()>,
) -> StackExit {
    let mut stack = match start_stack(cfg).await {
        Ok(s) => s,
        Err(exit) => return exit,
    };
    // The file may have changed between the caller's initial load and here.
    let mut seen = None;
    let mut pending: Option<Vec<u8>> = None;
    let mut read_failed = false;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let trial =
        tokio::time::sleep(TRIAL_LIMIT.unwrap_or(Duration::from_secs(365 * 24 * 3600 * 100)));
    tokio::pin!(shutdown, trial);
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            _ = &mut trial, if TRIAL_LIMIT.is_some() => break,
            _ = tick.tick() => {
                let bytes = match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(e) => { if !read_failed { tracing::warn!(error = %e, "config read failed; keeping active configuration"); } read_failed = true; pending = None; continue; }
                };
                read_failed = false;
                if seen.as_ref() == Some(&bytes) { pending = None; continue; }
                if pending.as_ref() != Some(&bytes) { pending = Some(bytes); continue; }
                // Load exactly the bytes that were debounced, rebasing paths.
                let result = String::from_utf8(bytes.clone()).map_err(|e| e.to_string())
                    .and_then(|text| gateway_config::load_str_at(&text, &path).map_err(|e| e.to_string()));
                match result {
                    Ok(cfg) => { if let Err(e) = stack.reload(cfg).await { tracing::error!(error = %e, "config reload failed"); } }
                    Err(e) => tracing::error!(error = %e, "config reload rejected; keeping active configuration"),
                }
                seen = Some(bytes);
                pending = None;
            }
        }
    }
    stack.shutdown().await;
    StackExit::Clean
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
    async fn reload_preserves_acquisition_time_but_does_not_restore_an_edited_source() {
        use mb_poller::{RawValue, RawValueSink};
        use tags_core::{TypedReader, TypedValue};
        let dir = tempfile::tempdir().unwrap();
        let mut file = serde_json::json!({
            "schema_version":"1", "gateway":{"data_dir":dir.path().join("data")},
            "opcua":{"enabled":false}, "poll_groups":[{"id":"p","period_ms":100}],
            "channels":[{"id":"c","enabled":false,"transport":{"type":"tcp","host":"127.0.0.1"},"devices":[{"id":"d","unit_id":1,"registers":[{"tag":"a","address":0,"poll_group":"p","function":"read_holding_registers","data_type":"u16","retentive":true}]}]}]
        });
        let mut stack = start_stack(gateway_config::load_str(&file.to_string()).unwrap())
            .await
            .unwrap();
        let ts = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mono = Instant::now();
        stack.cache.publish_batch(
            &[(mb_types::TagId(0), RawValue::Registers(Arc::from([5u16])))],
            ts,
            mono,
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while stack.typed.snapshot(mb_types::TagId(0)).unwrap().value != TypedValue::UInt(5) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        file["channels"][0]["devices"][0]["registers"][0]["formula"] = serde_json::json!("raw * 2");
        stack
            .reload(gateway_config::load_str(&file.to_string()).unwrap())
            .await
            .unwrap();
        let snap = stack.typed.snapshot(mb_types::TagId(0)).unwrap();
        assert_eq!(snap.value, TypedValue::Float(10.0));
        assert_eq!(snap.source_ts, ts);
        assert_eq!(snap.mono, mono);
        assert_eq!(
            stack
                .persist
                .load_retentive("a")
                .unwrap()
                .unwrap()
                .to_typed()
                .0,
            TypedValue::Float(10.0)
        );
        file["channels"][0]["devices"][0]["registers"][0]["address"] = serde_json::json!(1);
        stack
            .reload(gateway_config::load_str(&file.to_string()).unwrap())
            .await
            .unwrap();
        let snap = stack.typed.snapshot(mb_types::TagId(0)).unwrap();
        assert_eq!(
            snap.value,
            TypedValue::Absent,
            "edited source must not inherit the old retentive sample"
        );
        assert_eq!(snap.quality, mb_poller::Quality::Bad);
        assert!(stack.persist.load_retentive("a").unwrap().is_none());
        let revision = stack.revision();
        file["opcua"]["port"] = serde_json::json!(4850);
        assert!(stack
            .reload(gateway_config::load_str(&file.to_string()).unwrap())
            .await
            .unwrap_err()
            .contains("startup settings"));
        assert_eq!(stack.revision(), revision);
        stack.shutdown().await;
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
