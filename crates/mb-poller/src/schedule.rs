//! `PollWheel` (design §4): one `tokio::time::Interval` per **distinct
//! period** on the channel (typically 2–4), each set to
//! `MissedTickBehavior::Skip` so an overrun on a slow bus does not accumulate
//! a catch-up burst. [`PollWheel::next_due`] resolves when at least one
//! interval fires and returns the `(device_idx, group_idx)` batch due now,
//! ordered by group priority (higher first).

use std::future::poll_fn;
use std::task::Poll;
use std::time::Duration;

use tokio::time::{interval, Interval, MissedTickBehavior};

use crate::plan::ChannelPlan;

/// One scheduled `(priority, device_idx, group_idx)` unit of work.
type Entry = (i32, usize, usize);

pub struct PollWheel {
    /// One per distinct period.
    intervals: Vec<Interval>,
    /// Index-aligned with `intervals`: the work due when that interval fires.
    /// Only devices that actually have transactions in the group are listed.
    due_map: Vec<Vec<Entry>>,
}

impl PollWheel {
    pub fn new(plan: &ChannelPlan) -> Self {
        let mut periods: Vec<Duration> = plan.groups.iter().map(|(_, p)| *p).collect();
        periods.sort_unstable();
        periods.dedup();

        let mut due_map: Vec<Vec<Entry>> = vec![Vec::new(); periods.len()];
        for (group_idx, (_, period)) in plan.groups.iter().enumerate() {
            let slot = periods.iter().position(|p| p == period).expect("own period");
            let prio = plan.group_priorities[group_idx];
            for (device_idx, dev) in plan.devices.iter().enumerate() {
                if !dev.by_group[group_idx].1.is_empty() {
                    due_map[slot].push((prio, device_idx, group_idx));
                }
            }
        }

        let intervals = periods
            .into_iter()
            .map(|p| {
                // Interval panics on zero period; clamp defensively.
                let mut iv = interval(p.max(Duration::from_millis(1)));
                iv.set_missed_tick_behavior(MissedTickBehavior::Skip);
                iv
            })
            .collect();

        PollWheel { intervals, due_map }
    }

    /// Wait for the next tick. Every interval that is due *right now* is
    /// consumed in the same call, so simultaneously-due groups come back as
    /// one batch (sorted by descending priority; stable within a priority).
    ///
    /// Cancel-safe: a `Pending` poll consumes no ticks. With no groups on the
    /// channel this never resolves.
    pub async fn next_due(&mut self) -> Vec<(usize, usize)> {
        let fired: Vec<usize> = poll_fn(|cx| {
            let mut fired = Vec::new();
            for (i, iv) in self.intervals.iter_mut().enumerate() {
                if iv.poll_tick(cx).is_ready() {
                    fired.push(i);
                }
            }
            if fired.is_empty() {
                Poll::Pending
            } else {
                Poll::Ready(fired)
            }
        })
        .await;

        let mut batch: Vec<Entry> = fired
            .into_iter()
            .flat_map(|i| self.due_map[i].iter().copied())
            .collect();
        batch.sort_by_key(|(prio, _, _)| std::cmp::Reverse(*prio));
        batch.into_iter().map(|(_, d, g)| (d, g)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::build_all;

    /// fast = 100 ms / prio 0; slow = 300 ms / prio 5.
    /// devA: slow only. devB: fast + slow.
    fn plan() -> ChannelPlan {
        let cfg = gateway_config::load_str(
            r#"{
            "schema_version": "1",
            "poll_groups": [
                { "id": "fast", "period_ms": 100 },
                { "id": "slow", "period_ms": 300, "priority": 5 }
            ],
            "channels": [ {
                "id": "c",
                "transport": { "type": "tcp", "host": "h" },
                "devices": [
                    { "id": "devA", "unit_id": 1, "registers": [
                        { "tag": "a1", "poll_group": "slow", "function": "read_holding_registers", "address": 0, "data_type": "u16" }
                    ] },
                    { "id": "devB", "unit_id": 2, "registers": [
                        { "tag": "b1", "poll_group": "fast", "function": "read_holding_registers", "address": 0, "data_type": "u16" },
                        { "tag": "b2", "poll_group": "slow", "function": "read_holding_registers", "address": 10, "data_type": "u16" }
                    ] }
                ]
            } ]
        }"#,
        )
        .unwrap();
        build_all(&cfg).remove(0)
    }

    // group_idx 0 = fast (PollGroupId 0), 1 = slow. device_idx 0 = devA, 1 = devB.
    const FAST_B: (usize, usize) = (1, 0);
    const SLOW_A: (usize, usize) = (0, 1);
    const SLOW_B: (usize, usize) = (1, 1);

    #[tokio::test(start_paused = true)]
    async fn first_tick_is_immediate_and_priority_orders_the_batch() {
        let mut wheel = PollWheel::new(&plan());
        let due = wheel.next_due().await;
        // Both periods fire at t=0 in ONE batch; slow (prio 5) comes first.
        // devA has nothing in "fast", so no (0, 0) pair exists.
        assert_eq!(due, vec![SLOW_A, SLOW_B, FAST_B]);
    }

    #[tokio::test(start_paused = true)]
    async fn periods_fire_independently_on_their_cadence() {
        let mut wheel = PollWheel::new(&plan());
        let t0 = tokio::time::Instant::now();
        wheel.next_due().await; // consume the immediate t=0 batch

        // Paused clock auto-advances to the next timer deadline.
        assert_eq!(wheel.next_due().await, vec![FAST_B]); // t=100
        assert_eq!(wheel.next_due().await, vec![FAST_B]); // t=200
        assert_eq!(
            wheel.next_due().await,
            vec![SLOW_A, SLOW_B, FAST_B],
            "t=300: both due, slow first by priority"
        );
        assert_eq!(t0.elapsed(), Duration::from_millis(300));
    }

    #[tokio::test(start_paused = true)]
    async fn missed_ticks_are_skipped_not_bursted() {
        let mut wheel = PollWheel::new(&plan());
        wheel.next_due().await; // t=0

        // Stall the channel for 1 s (e.g. slow bus): 10 fast + 3 slow ticks missed.
        tokio::time::advance(Duration::from_millis(1000)).await;
        let due = wheel.next_due().await;
        // Each group fires exactly ONCE — no catch-up burst.
        assert_eq!(due, vec![SLOW_A, SLOW_B, FAST_B]);

        // The next fire is in the future, aligned to the skip behavior — not
        // another immediate replay.
        let before = tokio::time::Instant::now();
        let due = wheel.next_due().await;
        assert!(!due.is_empty());
        assert!(tokio::time::Instant::now() > before);
    }
}
