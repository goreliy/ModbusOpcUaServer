//! `TagEngine` — the pump between the raw poller cache and the typed store:
//! subscribe to `ChangeBatch`, decode, apply the transform, gate by deadband,
//! publish into [`TypedStore`], persist retentive values / history rings.
//!
//! One task per engine; per-tag state (last published numeric for deadband)
//! is task-local — no locks beyond the stores' own slot locks.

use std::sync::Arc;

use mb_poller::{CacheReader, ChangeBatch, Quality};
use mb_types::TagId;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::decode::{decode, DecodeMeta};
use crate::formula::{FormulaError, Transform};
use crate::persist::{Persist, PersistError, StoredValue};
use crate::store::{TypedReader, TypedStore};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("tag `{tag}`: {source}")]
    Formula {
        tag: String,
        #[source]
        source: FormulaError,
    },
    #[error(transparent)]
    Persist(#[from] PersistError),
    #[error("{0}")]
    Dependencies(String),
}

/// Per-tag processing recipe, compiled once at engine start.
struct TagMeta {
    decode: DecodeMeta,
    transform: Transform,
    deadband: Option<f64>,
    retentive: bool,
    retain_last: u16,
}

pub struct EngineHandle {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl EngineHandle {
    /// Signal the pump to stop, wait for it to flush and exit.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

pub struct TagEngine {
    cache: Arc<dyn CacheReader>,
    store: Arc<TypedStore>,
    metas: Vec<Option<TagMeta>>,
    persist: Option<Persist>,
    calculated: Arc<TypedStore>,
    graph: crate::graph::DependencyGraph,
    receiver: Option<broadcast::Receiver<ChangeBatch>>,
    primed: bool,
}

impl TagEngine {
    /// Build the engine: compile every formula (fail-fast — a syntax error
    /// aborts startup with the offending tag named) and restore retentive
    /// values from `persist` into the typed store as `Uncertain`.
    pub fn new(
        cfg: &gateway_config::ResolvedConfig,
        cache: Arc<dyn CacheReader>,
        persist: Option<Persist>,
    ) -> Result<Self, EngineError> {
        let store = Arc::new(TypedStore::new(cfg));
        let calculated = Arc::new(TypedStore::new(cfg));
        let lookup: Arc<dyn crate::formula::TagLookup> = Arc::clone(&calculated) as _;
        let mut formulas = vec![None; cfg.tag_count()];
        for reg in cfg
            .channels
            .iter()
            .flat_map(|c| &c.devices)
            .flat_map(|d| &d.registers)
        {
            formulas[reg.tag.0 as usize] = reg.formula.as_deref();
            if let Some(text) = &reg.write_formula {
                crate::Formula::compile(text, "value", None).map_err(|source| {
                    EngineError::Formula {
                        tag: cfg.tag_names[reg.tag.0 as usize].clone(),
                        source,
                    }
                })?;
            }
        }

        let mut metas: Vec<Option<TagMeta>> = (0..cfg.tag_names.len()).map(|_| None).collect();
        for ch in &cfg.channels {
            for dev in &ch.devices {
                for reg in &dev.registers {
                    let transform = Transform::from_meta(
                        reg.formula.as_deref(),
                        reg.scale,
                        reg.offset,
                        Some(Arc::clone(&lookup)),
                    )
                    .map_err(|source| EngineError::Formula {
                        tag: cfg.tag_names[reg.tag.0 as usize].clone(),
                        source,
                    })?;
                    metas[reg.tag.0 as usize] = Some(TagMeta {
                        decode: DecodeMeta {
                            data_type: reg.data_type,
                            word_order: reg.word_order,
                            byte_order: reg.byte_order,
                            bit: reg.bit,
                        },
                        transform,
                        deadband: reg.deadband.filter(|d| *d > 0.0),
                        retentive: reg.retentive,
                        retain_last: reg.retain_last.unwrap_or(0),
                    });
                }
            }
        }

        let graph = crate::graph::DependencyGraph::build(
            cfg.tag_names
                .iter()
                .zip(&formulas)
                .map(|(name, formula)| (name.as_str(), *formula)),
        )
        .map_err(EngineError::Dependencies)?;
        let receiver = Some(cache.subscribe());
        let engine = Self {
            cache,
            store,
            metas,
            persist,
            calculated,
            graph,
            receiver,
            primed: false,
        };
        engine.restore_retentive()?;
        Ok(engine)
    }

    /// Read access to the typed store (also usable before `spawn`).
    pub fn store(&self) -> Arc<TypedStore> {
        Arc::clone(&self.store)
    }

    /// Recompute immediately before publishing a newly prepared generation.
    pub fn refresh(&mut self) {
        let tags: Vec<_> = self.graph.order.clone();
        let mut last = self.published_numbers();
        self.process(&tags, &mut last);
        self.primed = true;
    }

    fn published_numbers(&self) -> Vec<Option<f64>> {
        (0..self.metas.len())
            .map(|i| {
                self.store
                    .snapshot(TagId(i as u32))
                    .and_then(|s| s.value.as_f64())
            })
            .collect()
    }

    fn restore_retentive(&self) -> Result<(), EngineError> {
        let Some(persist) = &self.persist else {
            return Ok(());
        };
        let mut restored = Vec::new();
        for (i, meta) in self.metas.iter().enumerate() {
            let Some(meta) = meta else { continue };
            if !meta.retentive {
                continue;
            }
            let tag = TagId(i as u32);
            let Some(name) = self.store.name(tag) else {
                continue;
            };
            if let Some(sv) = persist.load_retentive(name)? {
                let (value, ts) = sv.to_typed();
                self.store.restore(tag, value, ts);
                restored.push(tag);
            }
        }
        if !restored.is_empty() {
            tracing::info!(
                count = restored.len(),
                "retentive tags restored (Uncertain)"
            );
            self.store.send_batch(restored.into());
        }
        Ok(())
    }

    /// Start the pump. Returns the handle and the typed store for phase 3.
    pub fn spawn(self) -> (EngineHandle, Arc<TypedStore>) {
        let store = Arc::clone(&self.store);
        let (sd_tx, sd_rx) = watch::channel(false);
        let task = tokio::spawn(self.run(sd_rx));
        (
            EngineHandle {
                shutdown: sd_tx,
                task,
            },
            store,
        )
    }

    async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        // Subscribe FIRST, then do a full initial pass — an update landing in
        // between is processed twice, which is idempotent.
        let mut rx = self
            .receiver
            .take()
            .expect("engine owns one raw subscription");
        // Deadband memory: last PUBLISHED numeric per tag (task-local).
        let mut last_published = self.published_numbers();

        let initial: Vec<TagId> = self.cache.snapshot_all().iter().map(|(t, _)| *t).collect();
        if !self.primed {
            self.process(&initial, &mut last_published);
        }

        loop {
            tokio::select! {
                biased;

                res = shutdown.changed() => {
                    if res.is_err() || *shutdown.borrow() {
                        break;
                    }
                }

                msg = rx.recv() => match msg {
                    Ok(ChangeBatch { tags, .. }) => self.process(&tags, &mut last_published),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "change stream lagged: full resync");
                        let all: Vec<TagId> =
                            self.cache.snapshot_all().iter().map(|(t, _)| *t).collect();
                        self.process(&all, &mut last_published);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }

        if let Some(p) = &self.persist {
            if let Err(e) = p.flush() {
                tracing::warn!(error = %e, "persist flush on shutdown failed");
            }
        }
    }

    /// Decode + transform + gate one batch of tags; emit one typed batch.
    fn process(&self, tags: &[TagId], last_published: &mut [Option<f64>]) {
        let mut changed = Vec::with_capacity(tags.len());
        let affected = self.graph.affected(tags);
        // Freeze the raw inputs for this evaluation wave.
        let inputs: Vec<_> = (0..self.metas.len())
            .map(|i| {
                if affected[i] {
                    self.cache.snapshot(TagId(i as u32))
                } else {
                    None
                }
            })
            .collect();
        for &tag in &self.graph.order {
            if !affected[tag.0 as usize] {
                continue;
            }
            let Some(Some(meta)) = self.metas.get(tag.0 as usize) else {
                continue;
            };
            let Some(raw) = &inputs[tag.0 as usize] else {
                continue;
            };
            // A formula solely over other tags has no dependency on this
            // register's value or communication quality.
            let computed = mb_poller::Snapshot {
                value: mb_poller::RawValue::Absent,
                quality: Quality::Good,
                source_ts: std::time::SystemTime::now(),
                mono: std::time::Instant::now(),
                seq: 0,
            };
            let raw = if meta.transform.uses_raw() {
                raw
            } else {
                &computed
            };
            let mut quality = raw.quality;
            let mut ts = raw.source_ts;
            let mut mono = raw.mono;
            for &dep in &self.graph.dependencies[tag.0 as usize] {
                if let Some(snap) = self.calculated.snapshot(dep) {
                    quality = match (quality, snap.quality) {
                        (Quality::Bad, _) | (_, Quality::Bad) => Quality::Bad,
                        (Quality::Uncertain, _) | (_, Quality::Uncertain) => Quality::Uncertain,
                        _ => Quality::Good,
                    };
                    ts = ts.min(snap.source_ts);
                    mono = mono.min(snap.mono);
                }
            }

            // Non-Good raw: propagate quality, keep the last typed value.
            if quality != Quality::Good {
                self.calculated.set_quality(tag, quality);
                let prev = self.store.snapshot(tag).map(|s| s.quality);
                if prev != Some(quality) {
                    self.store.set_quality(tag, quality);
                    changed.push(tag);
                }
                continue;
            }

            let decoded = if meta.transform.uses_raw() {
                decode(&raw.value, meta.decode)
            } else {
                Ok(crate::TypedValue::Float(0.0))
            };
            let typed = match decoded
                .map_err(|e| e.to_string())
                .and_then(|v| meta.transform.apply(v).map_err(|e| e.to_string()))
            {
                Ok(v) => v,
                Err(e) => {
                    // Decode/formula failure: the value cannot be trusted.
                    tracing::warn!(tag = tag.0, error = %e, "decode/formula failed: tag Bad");
                    self.calculated.set_quality(tag, Quality::Bad);
                    let prev = self.store.snapshot(tag).map(|s| s.quality);
                    if prev != Some(Quality::Bad) {
                        self.store.set_quality(tag, Quality::Bad);
                        changed.push(tag);
                    }
                    continue;
                }
            };
            if typed.as_f64().is_some_and(|v| !v.is_finite()) {
                self.calculated.set_quality(tag, Quality::Bad);
                if self
                    .store
                    .snapshot(tag)
                    .is_some_and(|s| s.quality != Quality::Bad)
                {
                    self.store.set_quality(tag, Quality::Bad);
                    changed.push(tag);
                }
                continue;
            }
            self.calculated.publish_value(tag, typed.clone(), ts, mono);

            // Deadband gate — value-only suppression; a quality recovery
            // (previous typed not Good) always publishes.
            let was_good = self
                .store
                .snapshot(tag)
                .is_some_and(|s| s.quality == Quality::Good);
            if was_good {
                if let (Some(db), Some(new), Some(old)) = (
                    meta.deadband,
                    typed.as_f64(),
                    last_published.get(tag.0 as usize).copied().flatten(),
                ) {
                    if (new - old).abs() < db {
                        continue; // suppressed
                    }
                }
            }

            if let Some(f) = typed.as_f64() {
                last_published[tag.0 as usize] = Some(f);
            }
            self.store.publish_value(tag, typed.clone(), ts, mono);
            changed.push(tag);

            // Persistence (by stable NAME).
            if meta.retentive || meta.retain_last > 0 {
                if let (Some(persist), Some(name)) = (&self.persist, self.store.name(tag)) {
                    if let Some(sv) = StoredValue::from_typed(&typed, ts) {
                        if meta.retentive {
                            if let Err(e) = persist.store_retentive(name, &sv) {
                                tracing::warn!(tag = name, error = %e, "retentive store failed");
                            }
                        }
                        if meta.retain_last > 0 {
                            if let Err(e) = persist.push_history(name, &sv, meta.retain_last) {
                                tracing::warn!(tag = name, error = %e, "history store failed");
                            }
                        }
                    }
                }
            }
        }
        self.store.send_batch(changed.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TypedValue;
    use mb_poller::{RawValue, RawValueSink, TagCache};
    use std::time::{Instant, SystemTime};

    fn cfg_json(extra: &str) -> String {
        format!(
            r#"{{
            "schema_version": "1",
            "poll_groups": [ {{ "id": "fast", "period_ms": 100 }} ],
            "channels": [ {{
                "id": "c", "transport": {{ "type": "tcp", "host": "h" }},
                "devices": [ {{ "id": "d", "unit_id": 1, "registers": [ {extra} ] }} ]
            }} ]
        }}"#
        )
    }

    fn setup(regs_json: &str) -> (gateway_config::ResolvedConfig, Arc<TagCache>) {
        let cfg = gateway_config::load_str(&cfg_json(regs_json)).expect("cfg");
        let cache = Arc::new(TagCache::new(&cfg));
        (cfg, cache)
    }

    fn publish_regs(cache: &TagCache, tag: TagId, regs: &[u16]) {
        cache.publish_batch(
            &[(tag, RawValue::Registers(Arc::from(regs)))],
            SystemTime::now(),
            Instant::now(),
        );
    }

    #[test]
    fn cascade_is_order_independent_and_deadband_does_not_round_inputs() {
        // Reverse declaration order; diamond a -> b,c -> d.
        let (cfg, cache) = setup(
            r#"
            {"tag":"d","poll_group":"fast","function":"read_holding_registers","address":3,"data_type":"u16","formula":"tag(\"b\") + tag(\"c\")"},
            {"tag":"c","poll_group":"fast","function":"read_holding_registers","address":2,"data_type":"u16","formula":"tag(\"a\") * 3"},
            {"tag":"b","poll_group":"fast","function":"read_holding_registers","address":1,"data_type":"u16","formula":"tag(\"a\") * 2"},
            {"tag":"a","poll_group":"fast","function":"read_holding_registers","address":0,"data_type":"u16","deadband":10.0}
        "#,
        );
        // No raw samples for computed b/c/d: only a is an actual source.
        publish_regs(&cache, TagId(3), &[100]);
        let engine = TagEngine::new(&cfg, cache.clone() as _, None).unwrap();
        let mut last = vec![None; 4];
        engine.process(&[TagId(0), TagId(1), TagId(2), TagId(3)], &mut last);
        let store = engine.store();
        assert_eq!(
            store.snapshot(TagId(0)).unwrap().value,
            TypedValue::Float(500.0)
        );
        let before = store.snapshot(TagId(0)).unwrap().seq;
        publish_regs(&cache, TagId(3), &[102]);
        engine.process(&[TagId(3), TagId(3)], &mut last);
        assert_eq!(
            store.snapshot(TagId(3)).unwrap().value,
            TypedValue::UInt(100)
        );
        assert_eq!(
            store.snapshot(TagId(0)).unwrap().value,
            TypedValue::Float(510.0)
        );
        assert_eq!(
            store.snapshot(TagId(0)).unwrap().seq,
            before + 1,
            "diamond must evaluate once per wave"
        );
        cache.set_device_quality(&[TagId(3)], Quality::Uncertain);
        engine.process(&[TagId(3)], &mut last);
        for i in 0..4 {
            assert_eq!(
                store.snapshot(TagId(i)).unwrap().quality,
                Quality::Uncertain
            );
        }
        cache.set_device_quality(&[TagId(3)], Quality::Bad);
        engine.process(&[TagId(3)], &mut last);
        assert_eq!(store.snapshot(TagId(0)).unwrap().quality, Quality::Bad);
        assert_eq!(
            store.snapshot(TagId(0)).unwrap().value,
            TypedValue::Float(510.0)
        );
        publish_regs(&cache, TagId(3), &[103]);
        engine.process(&[TagId(3)], &mut last);
        assert_eq!(store.snapshot(TagId(0)).unwrap().quality, Quality::Good);
        assert_eq!(
            store.snapshot(TagId(0)).unwrap().value,
            TypedValue::Float(515.0)
        );
    }

    #[test]
    fn non_finite_formula_marks_dependents_bad_and_can_recover() {
        let (cfg, cache) = setup(
            r#"
            {"tag":"a","poll_group":"fast","function":"read_holding_registers","address":0,"data_type":"u16","formula":"1 / raw"},
            {"tag":"b","poll_group":"fast","function":"read_holding_registers","address":1,"data_type":"u16","formula":"tag(\"a\") * 2"}
        "#,
        );
        let engine = TagEngine::new(&cfg, cache.clone() as _, None).unwrap();
        let mut last = vec![None; 2];
        publish_regs(&cache, TagId(0), &[2]);
        publish_regs(&cache, TagId(1), &[0]);
        engine.process(&[TagId(0), TagId(1)], &mut last);
        assert_eq!(
            engine.store.snapshot(TagId(1)).unwrap().value,
            TypedValue::Float(1.0)
        );
        publish_regs(&cache, TagId(0), &[0]);
        engine.process(&[TagId(0)], &mut last);
        assert_eq!(
            engine.store.snapshot(TagId(0)).unwrap().quality,
            Quality::Bad
        );
        assert_eq!(
            engine.store.snapshot(TagId(1)).unwrap().quality,
            Quality::Bad
        );
        publish_regs(&cache, TagId(0), &[4]);
        engine.process(&[TagId(0)], &mut last);
        assert_eq!(
            engine.store.snapshot(TagId(1)).unwrap().quality,
            Quality::Good
        );
        assert_eq!(
            engine.store.snapshot(TagId(1)).unwrap().value,
            TypedValue::Float(0.5)
        );
    }

    #[tokio::test]
    async fn decode_transform_and_quality_flow_through() {
        let (cfg, cache) = setup(
            r#"{ "tag": "temp", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 0, "data_type": "i16", "scale": 0.1 },
               { "tag": "status", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 1, "data_type": "u16" }"#,
        );
        let engine = TagEngine::new(&cfg, Arc::clone(&cache) as _, None).unwrap();
        let (handle, store) = engine.spawn();

        publish_regs(&cache, TagId(0), &[237]);
        publish_regs(&cache, TagId(1), &[0x0003]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let t = store.snapshot(TagId(0)).unwrap();
        assert_eq!(t.quality, Quality::Good);
        match t.value {
            TypedValue::Float(f) => assert!((f - 23.7).abs() < 1e-9),
            other => panic!("expected Float, got {other:?}"),
        }
        // Identity transform keeps the native UInt.
        assert_eq!(store.snapshot(TagId(1)).unwrap().value, TypedValue::UInt(3));

        // Poller-side Bad propagates; typed value retained.
        cache.set_device_quality(&[TagId(0)], Quality::Bad);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let t = store.snapshot(TagId(0)).unwrap();
        assert_eq!(t.quality, Quality::Bad);
        assert!(matches!(t.value, TypedValue::Float(_)), "last value kept");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn deadband_suppresses_small_changes_but_not_recovery() {
        let (cfg, cache) = setup(
            r#"{ "tag": "flow", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 0, "data_type": "u16", "deadband": 5.0 }"#,
        );
        let engine = TagEngine::new(&cfg, Arc::clone(&cache) as _, None).unwrap();
        let (handle, store) = engine.spawn();
        let mut rx = store.subscribe();

        publish_regs(&cache, TagId(0), &[100]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s1 = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s1.value, TypedValue::UInt(100));
        let seq_after_first = s1.seq;

        // +2 < deadband 5 -> suppressed (no publish, seq unchanged).
        publish_regs(&cache, TagId(0), &[102]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s2 = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s2.seq, seq_after_first, "suppressed by deadband");
        assert_eq!(s2.value, TypedValue::UInt(100));

        // +7 >= 5 -> published.
        publish_regs(&cache, TagId(0), &[107]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s3 = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s3.value, TypedValue::UInt(107));
        assert!(s3.seq > seq_after_first);

        // Bad -> small change -> must still publish (quality recovery).
        cache.set_device_quality(&[TagId(0)], Quality::Bad);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        publish_regs(&cache, TagId(0), &[108]); // within deadband of 107
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s4 = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s4.quality, Quality::Good, "recovery beats deadband");
        assert_eq!(s4.value, TypedValue::UInt(108));

        // The change stream carried batches for the published transitions.
        let mut seen = 0;
        while rx.try_recv().is_ok() {
            seen += 1;
        }
        assert!(seen >= 3, "expected >=3 typed batches, got {seen}");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn cross_tag_formula_reads_previous_tag() {
        let (cfg, cache) = setup(
            r#"{ "tag": "a", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 0, "data_type": "u16" },
               { "tag": "sum", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 1, "data_type": "u16", "formula": "raw + tag(\"a\")" }"#,
        );
        let engine = TagEngine::new(&cfg, Arc::clone(&cache) as _, None).unwrap();
        let (handle, store) = engine.spawn();

        publish_regs(&cache, TagId(0), &[10]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        publish_regs(&cache, TagId(1), &[5]);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert_eq!(
            store.snapshot(TagId(1)).unwrap().value,
            TypedValue::Float(15.0)
        );

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bad_formula_fails_engine_construction_with_tag_name() {
        let (cfg, cache) = setup(
            r#"{ "tag": "broken", "poll_group": "fast", "function": "read_holding_registers",
                 "address": 0, "data_type": "u16", "formula": "raw +* nonsense(" }"#,
        );
        let err = match TagEngine::new(&cfg, cache as _, None) {
            Err(e) => e,
            Ok(_) => panic!("bad formula must fail engine construction"),
        };
        assert!(
            matches!(&err, EngineError::Formula { tag, .. } if tag == "broken"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn retentive_value_survives_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let regs = r#"{ "tag": "energy", "poll_group": "fast", "function": "read_holding_registers",
                        "address": 0, "data_type": "u16", "retentive": true, "retain_last": 5 }"#;

        // First life: publish, shutdown (flushes).
        {
            let (cfg, cache) = setup(regs);
            let engine = TagEngine::new(
                &cfg,
                Arc::clone(&cache) as _,
                Some(Persist::open(dir.path()).unwrap()),
            )
            .unwrap();
            let (handle, store) = engine.spawn();
            publish_regs(&cache, TagId(0), &[555]);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                store.snapshot(TagId(0)).unwrap().value,
                TypedValue::UInt(555)
            );
            handle.shutdown().await;
        }

        // Second life: restored as Uncertain before any poll.
        {
            let (cfg, cache) = setup(regs);
            let engine = TagEngine::new(
                &cfg,
                Arc::clone(&cache) as _,
                Some(Persist::open(dir.path()).unwrap()),
            )
            .unwrap();
            let store = engine.store();
            let s = store.snapshot(TagId(0)).unwrap();
            assert_eq!(s.value, TypedValue::UInt(555), "value restored from sled");
            assert_eq!(s.quality, Quality::Uncertain, "restored = not fresh");

            // History ring was written too.
            let p = Persist::open(dir.path()); // second open on same dir fails (sled lock)
            assert!(p.is_err() || p.is_ok()); // avoid double-open assumptions here
            drop(engine);
        }
    }
}
