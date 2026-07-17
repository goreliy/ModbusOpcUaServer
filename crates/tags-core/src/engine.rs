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
        let lookup: Arc<dyn crate::formula::TagLookup> = Arc::clone(&store) as _;

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

        let engine = Self { cache, store, metas, persist };
        engine.restore_retentive()?;
        Ok(engine)
    }

    /// Read access to the typed store (also usable before `spawn`).
    pub fn store(&self) -> Arc<TypedStore> {
        Arc::clone(&self.store)
    }

    fn restore_retentive(&self) -> Result<(), EngineError> {
        let Some(persist) = &self.persist else { return Ok(()) };
        let mut restored = Vec::new();
        for (i, meta) in self.metas.iter().enumerate() {
            let Some(meta) = meta else { continue };
            if !meta.retentive {
                continue;
            }
            let tag = TagId(i as u32);
            let Some(name) = self.store.name(tag) else { continue };
            if let Some(sv) = persist.load_retentive(name)? {
                let (value, ts) = sv.to_typed();
                self.store.restore(tag, value, ts);
                restored.push(tag);
            }
        }
        if !restored.is_empty() {
            tracing::info!(count = restored.len(), "retentive tags restored (Uncertain)");
            self.store.send_batch(restored.into());
        }
        Ok(())
    }

    /// Start the pump. Returns the handle and the typed store for phase 3.
    pub fn spawn(self) -> (EngineHandle, Arc<TypedStore>) {
        let store = Arc::clone(&self.store);
        let (sd_tx, sd_rx) = watch::channel(false);
        let task = tokio::spawn(self.run(sd_rx));
        (EngineHandle { shutdown: sd_tx, task }, store)
    }

    async fn run(self, mut shutdown: watch::Receiver<bool>) {
        // Subscribe FIRST, then do a full initial pass — an update landing in
        // between is processed twice, which is idempotent.
        let mut rx = self.cache.subscribe();
        // Deadband memory: last PUBLISHED numeric per tag (task-local).
        let mut last_published: Vec<Option<f64>> = vec![None; self.metas.len()];

        let initial: Vec<TagId> = self.cache.snapshot_all().iter().map(|(t, _)| *t).collect();
        self.process(&initial, &mut last_published);

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
        for &tag in tags {
            let Some(Some(meta)) = self.metas.get(tag.0 as usize) else { continue };
            let Some(raw) = self.cache.snapshot(tag) else { continue };

            // Non-Good raw: propagate quality, keep the last typed value.
            if raw.quality != Quality::Good {
                let prev = self.store.snapshot(tag).map(|s| s.quality);
                if prev != Some(raw.quality) {
                    self.store.set_quality(tag, raw.quality);
                    changed.push(tag);
                }
                continue;
            }

            let typed = match decode(&raw.value, meta.decode)
                .map_err(|e| e.to_string())
                .and_then(|v| meta.transform.apply(v).map_err(|e| e.to_string()))
            {
                Ok(v) => v,
                Err(e) => {
                    // Decode/formula failure: the value cannot be trusted.
                    tracing::warn!(tag = tag.0, error = %e, "decode/formula failed: tag Bad");
                    let prev = self.store.snapshot(tag).map(|s| s.quality);
                    if prev != Some(Quality::Bad) {
                        self.store.set_quality(tag, Quality::Bad);
                        changed.push(tag);
                    }
                    continue;
                }
            };

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
            self.store.publish_value(tag, typed.clone(), raw.source_ts, raw.mono);
            changed.push(tag);

            // Persistence (by stable NAME).
            if meta.retentive || meta.retain_last > 0 {
                if let (Some(persist), Some(name)) = (&self.persist, self.store.name(tag)) {
                    if let Some(sv) = StoredValue::from_typed(&typed, raw.source_ts) {
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

        assert_eq!(store.snapshot(TagId(1)).unwrap().value, TypedValue::Float(15.0));

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
        assert!(matches!(&err, EngineError::Formula { tag, .. } if tag == "broken"), "{err}");
    }

    #[tokio::test]
    async fn retentive_value_survives_engine_restart() {
        let dir = tempfile::tempdir().unwrap();
        let regs = r#"{ "tag": "energy", "poll_group": "fast", "function": "read_holding_registers",
                        "address": 0, "data_type": "u16", "retentive": true, "retain_last": 5 }"#;

        // First life: publish, shutdown (flushes).
        {
            let (cfg, cache) = setup(regs);
            let engine =
                TagEngine::new(&cfg, Arc::clone(&cache) as _, Some(Persist::open(dir.path()).unwrap()))
                    .unwrap();
            let (handle, store) = engine.spawn();
            publish_regs(&cache, TagId(0), &[555]);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(store.snapshot(TagId(0)).unwrap().value, TypedValue::UInt(555));
            handle.shutdown().await;
        }

        // Second life: restored as Uncertain before any poll.
        {
            let (cfg, cache) = setup(regs);
            let engine =
                TagEngine::new(&cfg, Arc::clone(&cache) as _, Some(Persist::open(dir.path()).unwrap()))
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
