//! `mb-poller` — poll scheduler, compile-time coalescing, tag cache and the
//! per-channel runtime (docs/phase1-design.md §4–§8).
//!
//! - [`cache`] — flat single-writer-per-slot `TagCache` behind the
//!   [`cache::RawValueSink`] / [`cache::CacheReader`] seams;
//! - [`plan`] / [`coalesce`] — `ResolvedConfig` -> `Vec<ChannelPlan>` compile
//!   step with the pure merge algorithm;
//! - [`schedule`] — `PollWheel`, one `Interval` per distinct period;
//! - [`device`] — `DeviceRuntime`: offline watchdog + adaptive de-coalescing;
//! - [`channel`] — `run_channel`, the one owning task per physical channel;
//! - [`command`] — the write path (`WriteCommand` over per-channel `mpsc`);
//! - [`metrics`] — per-channel `AtomicU64` counters.
//!
//! Entry point: [`Poller::spawn`] (bring your own [`RawValueSink`]) or
//! [`Poller::spawn_with_cache`] (owns a [`TagCache`]).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use mb_types::ChannelId;

pub mod cache;
pub mod channel;
pub mod coalesce;
pub mod command;
pub mod device;
pub mod metrics;
pub mod plan;
pub mod schedule;

pub use cache::{
    CacheReader, ChangeBatch, Quality, RawValue, RawValueSink, Snapshot, TagCache, TagSlot,
    CHANGE_CAPACITY,
};
pub use coalesce::{coalesce, Caps, Interval};
pub use command::WriteCommand;
pub use device::DeviceRuntime;
pub use metrics::{ChannelMetrics, MetricsSnapshot};
pub use plan::{build_all, ChannelPlan, DevicePlan, Field, Transaction};
pub use schedule::PollWheel;

/// Handle to a running poller: write-command senders, per-channel metrics and
/// graceful shutdown. Dropping the handle without calling
/// [`PollerHandle::shutdown`] STOPS every channel task (the internal shutdown
/// sender is dropped and the tasks exit at their next boundary) — hold the
/// handle for as long as polling should run (#29).
pub struct PollerHandle {
    /// OPC UA / MQTT -> channel task.
    writers: HashMap<ChannelId, mpsc::Sender<WriteCommand>>,
    metrics: HashMap<ChannelId, Arc<ChannelMetrics>>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl PollerHandle {
    /// Write-command sender for one channel (bounded `mpsc(64)`: natural
    /// backpressure; see design §8).
    pub fn writer(&self, ch: ChannelId) -> Option<mpsc::Sender<WriteCommand>> {
        self.writers.get(&ch).cloned()
    }

    /// All write senders (for the OPC UA layer to build its write plans).
    pub fn all_writers(&self) -> HashMap<ChannelId, mpsc::Sender<WriteCommand>> {
        self.writers.clone()
    }

    /// Per-channel counters (relaxed reads, diagnostics only).
    pub fn metrics(&self, ch: ChannelId) -> Option<Arc<ChannelMetrics>> {
        self.metrics.get(&ch).cloned()
    }

    /// Signal every channel task to stop and wait for them to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

pub struct Poller;

impl Poller {
    /// Compile the config and spawn one task per enabled channel. Must be
    /// called from within a tokio runtime. Borrows the config (#30): the
    /// caller keeps it for the OPC UA / MQTT / tags-core layers, which read
    /// per-register metadata from it after the poller is up.
    pub fn spawn(cfg: &gateway_config::ResolvedConfig, sink: Arc<dyn RawValueSink>) -> PollerHandle {
        let plans: Vec<ChannelPlan> = plan::build_all(cfg); // compile-time coalescing
        let (sd_tx, sd_rx) = watch::channel(false);
        let mut writers = HashMap::new();
        let mut metrics = HashMap::new();
        let mut tasks = Vec::new();
        for plan in plans {
            let (w_tx, w_rx) = mpsc::channel::<WriteCommand>(64);
            writers.insert(plan.id, w_tx);
            let m = Arc::new(ChannelMetrics::default());
            metrics.insert(plan.id, Arc::clone(&m));
            let sink = Arc::clone(&sink);
            let sd = sd_rx.clone();
            tasks.push(tokio::spawn(channel::run_channel(plan, sink, w_rx, sd, m)));
        }
        PollerHandle {
            writers,
            metrics,
            shutdown: sd_tx,
            tasks,
        }
    }

    /// Convenience: build the [`TagCache`] from the config, spawn the poller
    /// writing into it, and hand both back (the cache doubles as the
    /// [`CacheReader`] for OPC UA / MQTT / `tags-core`).
    pub fn spawn_with_cache(cfg: &gateway_config::ResolvedConfig) -> (PollerHandle, Arc<TagCache>) {
        let cache = Arc::new(TagCache::new(cfg));
        let handle = Self::spawn(cfg, Arc::clone(&cache) as Arc<dyn RawValueSink>);
        (handle, cache)
    }
}

#[cfg(test)]
pub(crate) mod test_sink {
    //! Recording `RawValueSink` shared by the unit tests.

    use std::sync::Mutex;
    use std::time::{Instant, SystemTime};

    use mb_types::TagId;

    use crate::cache::{Quality, RawValue, RawValueSink};

    #[derive(Default)]
    pub struct FakeSink {
        publishes: Mutex<Vec<Vec<(TagId, RawValue)>>>,
        quality: Mutex<Vec<(Vec<TagId>, Quality)>>,
        degraded: Mutex<Vec<Vec<TagId>>>,
    }

    impl FakeSink {
        pub fn publish_calls(&self) -> Vec<Vec<(TagId, RawValue)>> {
            self.publishes.lock().unwrap().clone()
        }
        pub fn quality_calls(&self) -> Vec<(Vec<TagId>, Quality)> {
            self.quality.lock().unwrap().clone()
        }
        #[allow(dead_code)]
        pub fn degrade_calls(&self) -> Vec<Vec<TagId>> {
            self.degraded.lock().unwrap().clone()
        }
    }

    impl RawValueSink for FakeSink {
        fn publish_batch(&self, updates: &[(TagId, RawValue)], _ts: SystemTime, _mono: Instant) {
            self.publishes.lock().unwrap().push(updates.to_vec());
        }
        fn set_device_quality(&self, tags: &[TagId], q: Quality) {
            self.quality.lock().unwrap().push((tags.to_vec(), q));
        }
        fn degrade_uncertain(&self, tags: &[TagId]) {
            self.degraded.lock().unwrap().push(tags.to_vec());
        }
    }
}
