//! Tag cache (design §6): flat `Box<[TagSlot]>` indexed by dense `TagId` —
//! no hashing, no shard lock, cache-friendly, and single-writer-per-slot **by
//! construction** (each channel owns an exclusive `TagId` range assigned in
//! `gateway-config::resolve`). The poller writes *raw* words/bits; `tags-core`
//! decodes in Phase 2.
//!
//! INVARIANT: every `parking_lot` write guard in this module is a pure field
//! assignment and is NEVER held across an `.await`. There are no awaits in
//! these methods — keep it that way.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use mb_types::TagId;
use tokio::sync::broadcast;

/// Maps to an OPC UA `StatusCode` later.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    Good,
    Uncertain,
    Bad,
}

/// RAW payload before decode. `Arc` so a read clones a pointer, not the data.
#[derive(Clone, Debug, PartialEq)]
pub enum RawValue {
    Bits(Arc<[bool]>),
    Registers(Arc<[u16]>),
    Raw(bytes::Bytes),
    /// Never-yet-read.
    Absent,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub value: RawValue,
    pub quality: Quality,
    /// For OPC UA `SourceTimestamp` (presentation only).
    pub source_ts: SystemTime,
    /// Monotonic time of the last VALUE write; staleness math uses THIS,
    /// never `SystemTime`. Quality-only flips do NOT touch it (#25).
    pub mono: Instant,
    /// Bumped per VALUE write only (#25) — quality-only transitions do not
    /// bump it, so do NOT dedupe `ChangeBatch` notifications via `seq`: the
    /// batch itself is the signal that something (value OR quality) changed.
    pub seq: u64,
}

/// One slot per tag. `parking_lot::RwLock`: sub-µs, no poisoning, no async.
pub struct TagSlot {
    inner: parking_lot::RwLock<Snapshot>,
}

/// Broadcast ring size for [`ChangeBatch`] notifications. Lossy on lag by
/// design: a lagged subscriber resyncs via [`CacheReader::snapshot_all`].
pub const CHANGE_CAPACITY: usize = 256;

/// Change notification is BATCHED PER TRANSACTION, not per tag. At 5000 tags
/// x 5 Hz a per-tag firehose (25k events/s) chronically lags subscribers; one
/// batch per coalesced read keeps event volume at ~request-rate. Lossy on
/// `Lagged` -> subscriber does a full `snapshot_all()` resync; never
/// back-pressures the poll loop.
#[derive(Clone, Debug)]
pub struct ChangeBatch {
    pub tags: Arc<[TagId]>,
    pub seq: u64,
}

pub struct TagCache {
    /// len = tag count, index = `TagId.0`.
    slots: Box<[TagSlot]>,
    /// Cold path: OPC UA node-id -> `TagId` at subscribe time.
    by_name: dashmap::DashMap<Box<str>, TagId>,
    changes: broadcast::Sender<ChangeBatch>,
    /// Batch counter, monotonic IN DELIVERY ORDER: reserve+send happen under
    /// [`TagCache::batch_lock`] (#24) so subscribers can detect gaps.
    batch_seq: AtomicU64,
    /// Serializes seq reservation with the broadcast send. Multiple channel
    /// tasks share one cache; without this, task A can reserve seq=5, get
    /// preempted, and deliver AFTER task B's seq=6 — a phantom "gap".
    batch_lock: parking_lot::Mutex<()>,
}

/// Poller-facing seam. `tags-core` / OPC UA / MQTT implement or wrap this.
pub trait RawValueSink: Send + Sync {
    /// Write one coalesced read's worth of tags in a single call.
    fn publish_batch(&self, updates: &[(TagId, RawValue)], ts: SystemTime, mono: Instant);
    /// Watchdog: flip many tags to `q`, keep last value, AND emit a change batch.
    fn set_device_quality(&self, tags: &[TagId], q: Quality);
    /// Transient single-read miss while the device is still online.
    fn degrade_uncertain(&self, tags: &[TagId]);
}

/// Consumer-facing seam.
pub trait CacheReader: Send + Sync {
    fn snapshot(&self, tag: TagId) -> Option<Snapshot>;
    fn resolve(&self, name: &str) -> Option<TagId>;
    fn subscribe(&self) -> broadcast::Receiver<ChangeBatch>;
    fn snapshot_all(&self) -> Vec<(TagId, Snapshot)>;
}

impl TagCache {
    /// Build from the resolved config: one slot per dense `TagId`, names from
    /// the resolved name table.
    pub fn new(cfg: &gateway_config::ResolvedConfig) -> Self {
        Self::from_names(cfg.tag_names.iter().map(String::as_str))
    }

    /// Build from an ordered name table; index = `TagId.0`.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let by_name = dashmap::DashMap::new();
        let mono = Instant::now();
        let slots: Vec<TagSlot> = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                by_name.insert(Box::from(name), TagId(i as u32));
                TagSlot {
                    inner: parking_lot::RwLock::new(Snapshot {
                        value: RawValue::Absent,
                        quality: Quality::Bad, // never-read = Bad until first Good poll
                        source_ts: SystemTime::UNIX_EPOCH,
                        mono,
                        seq: 0,
                    }),
                }
            })
            .collect();
        let (changes, _) = broadcast::channel(CHANGE_CAPACITY);
        TagCache {
            slots: slots.into_boxed_slice(),
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

    fn send_batch(&self, tags: Arc<[TagId]>) {
        // #24: reserve + send atomically so `seq` is monotonic in delivery
        // order across all channel tasks (sub-µs critical section, no await).
        let _g = self.batch_lock.lock();
        let seq = self.batch_seq.fetch_add(1, Ordering::Relaxed);
        // Lossy by design: Err just means "no subscribers right now".
        let _ = self.changes.send(ChangeBatch { tags, seq });
    }
}

impl RawValueSink for TagCache {
    fn publish_batch(&self, updates: &[(TagId, RawValue)], ts: SystemTime, mono: Instant) {
        let mut touched = Vec::with_capacity(updates.len());
        for (tag, val) in updates {
            let mut g = self.slots[tag.0 as usize].inner.write();
            g.value = val.clone();
            g.quality = Quality::Good;
            g.source_ts = ts;
            g.mono = mono;
            g.seq += 1; // pure assignment, no await; guard dropped at loop end
            touched.push(*tag);
        }
        self.send_batch(touched.into());
    }

    fn set_device_quality(&self, tags: &[TagId], q: Quality) {
        for t in tags {
            let mut g = self.slots[t.0 as usize].inner.write();
            g.quality = q; // last value kept (SCADA convention)
            // #25: `mono` is the time of the last VALUE — a quality flip must
            // not make a minutes-old value look 0 ms stale.
        }
        self.send_batch(Arc::from(tags)); // watchdog MUST notify
    }

    fn degrade_uncertain(&self, tags: &[TagId]) {
        for t in tags {
            let mut g = self.slots[t.0 as usize].inner.write();
            g.quality = Quality::Uncertain;
        }
        self.send_batch(Arc::from(tags));
    }
}

impl CacheReader for TagCache {
    fn snapshot(&self, tag: TagId) -> Option<Snapshot> {
        self.slots.get(tag.0 as usize).map(|s| s.inner.read().clone())
    }

    fn resolve(&self, name: &str) -> Option<TagId> {
        self.by_name.get(name).map(|r| *r.value())
    }

    fn subscribe(&self) -> broadcast::Receiver<ChangeBatch> {
        self.changes.subscribe()
    }

    fn snapshot_all(&self) -> Vec<(TagId, Snapshot)> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, s)| (TagId(i as u32), s.inner.read().clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    fn regs(vals: &[u16]) -> RawValue {
        RawValue::Registers(vals.to_vec().into())
    }

    #[test]
    fn publish_batch_updates_value_quality_seq_and_emits_one_batch() {
        let cache = TagCache::from_names(["a", "b", "c"]);
        let mut rx = cache.subscribe();
        let ts = SystemTime::now();
        let mono = Instant::now();

        cache.publish_batch(
            &[
                (TagId(0), regs(&[1, 2])),
                (TagId(1), RawValue::Bits(vec![true].into())),
            ],
            ts,
            mono,
        );

        let s0 = cache.snapshot(TagId(0)).unwrap();
        assert_eq!(s0.value, regs(&[1, 2]));
        assert_eq!(s0.quality, Quality::Good);
        assert_eq!(s0.seq, 1);
        assert_eq!(s0.source_ts, ts);
        assert_eq!(s0.mono, mono);
        let s1 = cache.snapshot(TagId(1)).unwrap();
        assert_eq!(s1.value, RawValue::Bits(vec![true].into()));
        assert_eq!(s1.seq, 1);
        // Untouched tag stays never-read.
        let s2 = cache.snapshot(TagId(2)).unwrap();
        assert_eq!(s2.value, RawValue::Absent);
        assert_eq!(s2.quality, Quality::Bad);
        assert_eq!(s2.seq, 0);

        // Exactly ONE batch for the whole transaction, carrying both tags.
        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.tags.as_ref(), &[TagId(0), TagId(1)]);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        // Second publish bumps per-slot seq again.
        cache.publish_batch(&[(TagId(0), regs(&[9]))], SystemTime::now(), Instant::now());
        assert_eq!(cache.snapshot(TagId(0)).unwrap().seq, 2);
        let second = rx.try_recv().unwrap();
        assert!(second.seq > batch.seq, "batch seq must be monotonic");
    }

    #[test]
    fn set_device_quality_flips_quality_keeps_value_and_notifies() {
        let cache = TagCache::from_names(["a", "b"]);
        cache.publish_batch(
            &[(TagId(0), regs(&[42])), (TagId(1), regs(&[7]))],
            SystemTime::now(),
            Instant::now(),
        );
        let mut rx = cache.subscribe(); // subscribe after publish: only sees the sweep

        cache.set_device_quality(&[TagId(0), TagId(1)], Quality::Bad);

        for t in [TagId(0), TagId(1)] {
            let s = cache.snapshot(t).unwrap();
            assert_eq!(s.quality, Quality::Bad);
            assert_ne!(s.value, RawValue::Absent, "last value must be kept");
        }
        assert_eq!(cache.snapshot(TagId(0)).unwrap().value, regs(&[42]));
        let batch = rx.try_recv().expect("watchdog sweep must emit a batch");
        assert_eq!(batch.tags.as_ref(), &[TagId(0), TagId(1)]);

        // Recovery path: quality can flip back without touching the value.
        cache.set_device_quality(&[TagId(0)], Quality::Good);
        assert_eq!(cache.snapshot(TagId(0)).unwrap().quality, Quality::Good);
        assert_eq!(cache.snapshot(TagId(0)).unwrap().value, regs(&[42]));
    }

    #[test]
    fn degrade_uncertain_keeps_value_and_notifies() {
        let cache = TagCache::from_names(["a"]);
        cache.publish_batch(&[(TagId(0), regs(&[5]))], SystemTime::now(), Instant::now());
        let mut rx = cache.subscribe();

        cache.degrade_uncertain(&[TagId(0)]);

        let s = cache.snapshot(TagId(0)).unwrap();
        assert_eq!(s.quality, Quality::Uncertain);
        assert_eq!(s.value, regs(&[5]));
        assert_eq!(rx.try_recv().unwrap().tags.as_ref(), &[TagId(0)]);
    }

    #[test]
    fn snapshot_all_covers_every_slot_in_tag_id_order() {
        let cache = TagCache::from_names(["x", "y", "z"]);
        cache.publish_batch(&[(TagId(1), regs(&[3]))], SystemTime::now(), Instant::now());

        let all = cache.snapshot_all();
        assert_eq!(all.len(), 3);
        let ids: Vec<u32> = all.iter().map(|(t, _)| t.0).collect();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(all[0].1.value, RawValue::Absent);
        assert_eq!(all[1].1.value, regs(&[3]));
        assert_eq!(all[1].1.quality, Quality::Good);
    }

    #[test]
    fn resolve_by_name() {
        let cache = TagCache::from_names(["plant.temp", "plant.pressure"]);
        assert_eq!(cache.resolve("plant.temp"), Some(TagId(0)));
        assert_eq!(cache.resolve("plant.pressure"), Some(TagId(1)));
        assert_eq!(cache.resolve("nope"), None);
        assert!(cache.snapshot(TagId(99)).is_none());
    }

    #[test]
    fn lagged_subscriber_gets_lagged_then_resyncs_via_snapshot_all() {
        let cache = TagCache::from_names(["a"]);
        let mut rx = cache.subscribe();

        let n = CHANGE_CAPACITY + 8;
        for i in 0..n {
            cache.publish_batch(
                &[(TagId(0), regs(&[i as u16]))],
                SystemTime::now(),
                Instant::now(),
            );
        }

        // Ring overflowed: the idle receiver observes Lagged, not silent loss.
        match rx.try_recv() {
            Err(TryRecvError::Lagged(missed)) => assert!(missed >= 1),
            other => panic!("expected Lagged, got {other:?}"),
        }

        // Resync: full snapshot carries the current state...
        let all = cache.snapshot_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.seq, n as u64);
        assert_eq!(all[0].1.value, regs(&[(n - 1) as u16]));

        // ...and the receiver keeps working from the oldest retained batch.
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn new_builds_from_resolved_config() {
        let json = r#"{
            "schema_version": "1",
            "poll_groups": [ { "id": "fast", "period_ms": 200 } ],
            "channels": [ {
                "id": "ch1",
                "transport": { "type": "tcp", "host": "10.0.0.1" },
                "devices": [ {
                    "id": "dev1", "unit_id": 1,
                    "registers": [
                        { "tag": "t1", "poll_group": "fast",
                          "function": "read_holding_registers",
                          "address": 0, "data_type": "u16" },
                        { "tag": "t2", "poll_group": "fast",
                          "function": "read_holding_registers",
                          "address": 1, "data_type": "u16" }
                    ]
                } ]
            } ]
        }"#;
        let rc = gateway_config::load_str(json).unwrap();
        let cache = TagCache::new(&rc);
        assert_eq!(cache.len(), rc.tag_count());
        assert_eq!(cache.resolve("t1"), Some(TagId(0)));
        assert_eq!(cache.resolve("t2"), Some(TagId(1)));
    }
}
