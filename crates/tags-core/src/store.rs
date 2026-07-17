//! Typed tag store — the phase-3-facing twin of `mb_poller::TagCache`:
//! flat single-writer slots indexed by dense `TagId`, batched change
//! notifications with delivery-order-monotonic `seq`, and a read seam
//! ([`TypedReader`]) for the OPC UA / MQTT layers.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use mb_poller::Quality;
use mb_types::TagId;
use tokio::sync::broadcast;

use crate::formula::TagLookup;
use crate::value::TypedValue;

/// Broadcast ring size for [`TypedBatch`] notifications; lossy on lag by
/// design — a lagged subscriber resyncs via [`TypedReader::snapshot_all`].
pub const TYPED_CHANGE_CAPACITY: usize = 256;

#[derive(Clone, Debug)]
pub struct TypedSnapshot {
    pub value: TypedValue,
    pub quality: Quality,
    /// For OPC UA `SourceTimestamp` (presentation only).
    pub source_ts: SystemTime,
    /// Monotonic time of the last VALUE write; staleness math uses this.
    /// Quality-only flips do not touch it.
    pub mono: Instant,
    /// Bumped per VALUE write only; quality-only transitions do not bump it —
    /// consume [`TypedBatch`] as the change signal, do not dedupe via `seq`.
    pub seq: u64,
}

/// One typed publish worth of tags (usually mirrors one poller `ChangeBatch`
/// minus deadband-suppressed members).
#[derive(Clone, Debug)]
pub struct TypedBatch {
    pub tags: Arc<[TagId]>,
    /// Monotonic in delivery order (reserve+send under one lock).
    pub seq: u64,
}

struct TypedSlot {
    inner: parking_lot::RwLock<TypedSnapshot>,
}

/// Consumer-facing seam for phase 3 (OPC UA / MQTT).
pub trait TypedReader: Send + Sync {
    fn snapshot(&self, tag: TagId) -> Option<TypedSnapshot>;
    fn resolve(&self, name: &str) -> Option<TagId>;
    /// Tag name by id (for topic/browse-name construction).
    fn name(&self, tag: TagId) -> Option<&str>;
    fn subscribe(&self) -> broadcast::Receiver<TypedBatch>;
    fn snapshot_all(&self) -> Vec<(TagId, TypedSnapshot)>;
}

pub struct TypedStore {
    slots: Box<[TypedSlot]>,
    /// index = TagId.0; owned copy of the resolved name table.
    names: Box<[Box<str>]>,
    by_name: dashmap_like::NameMap,
    changes: broadcast::Sender<TypedBatch>,
    batch_seq: AtomicU64,
    batch_lock: parking_lot::Mutex<()>,
}

/// Tiny name -> TagId map; read-mostly after construction.
mod dashmap_like {
    use mb_types::TagId;
    use std::collections::HashMap;

    pub struct NameMap(HashMap<Box<str>, TagId>);

    impl NameMap {
        pub fn new(names: &[Box<str>]) -> Self {
            Self(
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), TagId(i as u32)))
                    .collect(),
            )
        }
        pub fn get(&self, name: &str) -> Option<TagId> {
            self.0.get(name).copied()
        }
    }
}

impl TypedStore {
    pub fn new(cfg: &gateway_config::ResolvedConfig) -> Self {
        Self::from_names(cfg.tag_names.iter().map(String::as_str))
    }

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let names: Box<[Box<str>]> = names.into_iter().map(Box::from).collect();
        let mono = Instant::now();
        let slots: Vec<TypedSlot> = names
            .iter()
            .map(|_| TypedSlot {
                inner: parking_lot::RwLock::new(TypedSnapshot {
                    value: TypedValue::Absent,
                    quality: Quality::Bad,
                    source_ts: SystemTime::UNIX_EPOCH,
                    mono,
                    seq: 0,
                }),
            })
            .collect();
        let by_name = dashmap_like::NameMap::new(&names);
        let (changes, _) = broadcast::channel(TYPED_CHANGE_CAPACITY);
        TypedStore {
            slots: slots.into_boxed_slice(),
            names,
            by_name,
            changes,
            batch_seq: AtomicU64::new(0),
            batch_lock: parking_lot::Mutex::new(()),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Write a computed value; bumps `seq`, refreshes timestamps, Good.
    pub(crate) fn publish_value(&self, tag: TagId, value: TypedValue, ts: SystemTime, mono: Instant) {
        if let Some(slot) = self.slots.get(tag.0 as usize) {
            let mut g = slot.inner.write();
            g.value = value;
            g.quality = Quality::Good;
            g.source_ts = ts;
            g.mono = mono;
            g.seq += 1;
        }
    }

    /// Quality-only transition: value and `mono` are retained.
    pub(crate) fn set_quality(&self, tag: TagId, q: Quality) {
        if let Some(slot) = self.slots.get(tag.0 as usize) {
            slot.inner.write().quality = q;
        }
    }

    /// Restore a persisted value at boot (retentive tags): value present but
    /// not fresh -> Uncertain until the first live read.
    pub(crate) fn restore(&self, tag: TagId, value: TypedValue, ts: SystemTime) {
        if let Some(slot) = self.slots.get(tag.0 as usize) {
            let mut g = slot.inner.write();
            g.value = value;
            g.quality = Quality::Uncertain;
            g.source_ts = ts;
            // mono stays at construction time: the value IS stale.
        }
    }

    /// Emit one change batch (reserve + send atomically).
    pub(crate) fn send_batch(&self, tags: Arc<[TagId]>) {
        if tags.is_empty() {
            return;
        }
        let _g = self.batch_lock.lock();
        let seq = self.batch_seq.fetch_add(1, Ordering::Relaxed);
        let _ = self.changes.send(TypedBatch { tags, seq });
    }
}

impl TypedReader for TypedStore {
    fn snapshot(&self, tag: TagId) -> Option<TypedSnapshot> {
        self.slots.get(tag.0 as usize).map(|s| s.inner.read().clone())
    }

    fn resolve(&self, name: &str) -> Option<TagId> {
        self.by_name.get(name)
    }

    fn name(&self, tag: TagId) -> Option<&str> {
        self.names.get(tag.0 as usize).map(AsRef::as_ref)
    }

    fn subscribe(&self) -> broadcast::Receiver<TypedBatch> {
        self.changes.subscribe()
    }

    fn snapshot_all(&self) -> Vec<(TagId, TypedSnapshot)> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, s)| (TagId(i as u32), s.inner.read().clone()))
            .collect()
    }
}

/// `tag("name")` in formulas reads the CURRENT typed value: only Good,
/// numeric values participate — Bad/Uncertain/Absent read as unknown so a
/// formula cannot silently consume stale garbage.
impl TagLookup for TypedStore {
    fn numeric_value(&self, name: &str) -> Option<f64> {
        let id = self.resolve(name)?;
        let snap = self.snapshot(id)?;
        if snap.quality != Quality::Good {
            return None;
        }
        snap.value.as_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_and_snapshot_round_trip() {
        let store = TypedStore::from_names(["a", "b"]);
        assert_eq!(store.len(), 2);
        let ts = SystemTime::now();
        let mono = Instant::now();

        store.publish_value(TagId(0), TypedValue::Float(1.5), ts, mono);
        let s = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s.value, TypedValue::Float(1.5));
        assert_eq!(s.quality, Quality::Good);
        assert_eq!(s.seq, 1);

        // Quality flip keeps value + mono, does not bump seq.
        store.set_quality(TagId(0), Quality::Bad);
        let s2 = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s2.value, TypedValue::Float(1.5));
        assert_eq!(s2.quality, Quality::Bad);
        assert_eq!(s2.seq, 1);
        assert_eq!(s2.mono, s.mono);
    }

    #[test]
    fn resolve_and_names() {
        let store = TypedStore::from_names(["x.y", "z"]);
        assert_eq!(store.resolve("x.y"), Some(TagId(0)));
        assert_eq!(store.resolve("nope"), None);
        assert_eq!(store.name(TagId(1)), Some("z"));
        assert_eq!(store.name(TagId(9)), None);
    }

    #[test]
    fn batches_are_seq_monotonic_and_empty_batches_dropped() {
        let store = TypedStore::from_names(["a"]);
        let mut rx = store.subscribe();
        store.send_batch(Arc::from([TagId(0)].as_slice()));
        store.send_batch(Arc::from([] as [TagId; 0]));
        store.send_batch(Arc::from([TagId(0)].as_slice()));
        let b1 = rx.try_recv().unwrap();
        let b2 = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err(), "empty batch must not be sent");
        assert_eq!(b1.seq + 1, b2.seq);
    }

    #[test]
    fn restore_marks_uncertain_and_lookup_ignores_non_good() {
        let store = TypedStore::from_names(["r", "g"]);
        store.restore(TagId(0), TypedValue::Float(42.0), SystemTime::now());
        let s = store.snapshot(TagId(0)).unwrap();
        assert_eq!(s.value, TypedValue::Float(42.0));
        assert_eq!(s.quality, Quality::Uncertain);

        // TagLookup: Uncertain restored value must NOT feed formulas...
        assert_eq!(store.numeric_value("r"), None);
        // ...but a Good published one does.
        store.publish_value(TagId(1), TypedValue::UInt(7), SystemTime::now(), Instant::now());
        assert_eq!(store.numeric_value("g"), Some(7.0));
        // Text is non-numeric even when Good.
        store.publish_value(TagId(1), TypedValue::Text("s".into()), SystemTime::now(), Instant::now());
        assert_eq!(store.numeric_value("g"), None);
    }
}
