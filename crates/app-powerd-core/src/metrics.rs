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
}

/// Global metrics instance.
pub static METRICS: Metrics = Metrics::new();
