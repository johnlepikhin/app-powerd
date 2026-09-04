use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Global daemon metrics using atomic counters.
pub struct Metrics {
    pub apps_frozen_total: AtomicU64,
    pub apps_thawed_total: AtomicU64,
    pub apps_throttled_total: AtomicU64,
    pub apps_unthrottled_total: AtomicU64,
    pub focus_changes_total: AtomicU64,
    pub guard_blocks_total: AtomicU64,
    pub config_reloads_total: AtomicU64,
    pub time_in_frozen_ms: AtomicU64,
    pub time_in_throttled_ms: AtomicU64,

    // Visibility into the mechanisms that keep the daemon's model of the system
    // honest. They exist because the log deliberately stays quiet when
    // everything works: without counters there is no way to distinguish "the
    // reconcile sweep is running and finding nothing" from "the reconcile sweep
    // stopped running".
    /// Reconcile sweeps completed.
    pub reconcile_ticks_total: AtomicU64,
    /// Dead PIDs dropped from tracked applications.
    pub pids_reaped_total: AtomicU64,
    /// Applications removed because every process they owned had exited.
    pub apps_removed_stale_total: AtomicU64,
    /// Suspend attempts refused because the target is protected.
    pub protection_blocks_total: AtomicU64,
    /// Processes released on startup from a previous run's journal.
    pub journal_recovered_total: AtomicU64,
    /// Journal entries dropped because all their processes had exited.
    pub journal_stale_dropped_total: AtomicU64,
    /// Log messages withheld by the rate limiter.
    pub warns_suppressed_total: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub const fn new() -> Self {
        Self {
            apps_frozen_total: AtomicU64::new(0),
            apps_thawed_total: AtomicU64::new(0),
            apps_throttled_total: AtomicU64::new(0),
            apps_unthrottled_total: AtomicU64::new(0),
            focus_changes_total: AtomicU64::new(0),
            guard_blocks_total: AtomicU64::new(0),
            config_reloads_total: AtomicU64::new(0),
            time_in_frozen_ms: AtomicU64::new(0),
            time_in_throttled_ms: AtomicU64::new(0),
            reconcile_ticks_total: AtomicU64::new(0),
            pids_reaped_total: AtomicU64::new(0),
            apps_removed_stale_total: AtomicU64::new(0),
            protection_blocks_total: AtomicU64::new(0),
            journal_recovered_total: AtomicU64::new(0),
            journal_stale_dropped_total: AtomicU64::new(0),
            warns_suppressed_total: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            apps_frozen_total: self.apps_frozen_total.load(Ordering::Relaxed),
            apps_thawed_total: self.apps_thawed_total.load(Ordering::Relaxed),
            apps_throttled_total: self.apps_throttled_total.load(Ordering::Relaxed),
            apps_unthrottled_total: self.apps_unthrottled_total.load(Ordering::Relaxed),
            focus_changes_total: self.focus_changes_total.load(Ordering::Relaxed),
            guard_blocks_total: self.guard_blocks_total.load(Ordering::Relaxed),
            config_reloads_total: self.config_reloads_total.load(Ordering::Relaxed),
            time_in_frozen_ms: self.time_in_frozen_ms.load(Ordering::Relaxed),
            time_in_throttled_ms: self.time_in_throttled_ms.load(Ordering::Relaxed),
            reconcile_ticks_total: self.reconcile_ticks_total.load(Ordering::Relaxed),
            pids_reaped_total: self.pids_reaped_total.load(Ordering::Relaxed),
            apps_removed_stale_total: self.apps_removed_stale_total.load(Ordering::Relaxed),
            protection_blocks_total: self.protection_blocks_total.load(Ordering::Relaxed),
            journal_recovered_total: self.journal_recovered_total.load(Ordering::Relaxed),
            journal_stale_dropped_total: self.journal_stale_dropped_total.load(Ordering::Relaxed),
            warns_suppressed_total: self.warns_suppressed_total.load(Ordering::Relaxed),
        }
    }
}

/// Serializable snapshot of metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSnapshot {
    pub apps_frozen_total: u64,
    pub apps_thawed_total: u64,
    pub apps_throttled_total: u64,
    pub apps_unthrottled_total: u64,
    pub focus_changes_total: u64,
    pub guard_blocks_total: u64,
    pub config_reloads_total: u64,
    pub time_in_frozen_ms: u64,
    pub time_in_throttled_ms: u64,
    // Added in 2.0.0. `#[serde(default)]` so a newer CLI can still read a
    // response from an older daemon that does not send them.
    #[serde(default)]
    pub reconcile_ticks_total: u64,
    #[serde(default)]
    pub pids_reaped_total: u64,
    #[serde(default)]
    pub apps_removed_stale_total: u64,
    #[serde(default)]
    pub protection_blocks_total: u64,
    #[serde(default)]
    pub journal_recovered_total: u64,
    #[serde(default)]
    pub journal_stale_dropped_total: u64,
    #[serde(default)]
    pub warns_suppressed_total: u64,
}

/// Global metrics instance.
pub static METRICS: Metrics = Metrics::new();
