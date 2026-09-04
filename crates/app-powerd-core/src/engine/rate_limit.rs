//! Suppression of repeated log messages.
//!
//! A daemon that runs for months turns any per-cycle warning into an unbounded
//! log: a single closed application once produced 62 641 identical lines. The
//! underlying causes are fixed elsewhere; this is the backstop that keeps a
//! *new* recurring condition from doing the same thing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::metrics::METRICS;
use std::sync::atomic::Ordering;

/// Upper bound on tracked keys.
///
/// Keys embed PIDs, which the kernel hands out monotonically, so without a cap
/// the map would grow for as long as the daemon lives — the same unbounded
/// accumulation this crate is busy removing elsewhere.
const MAX_ENTRIES: usize = 1024;

/// What a suppressed message is about, so unrelated conditions do not silence
/// each other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LogKey {
    pub app_id: String,
    pub pid: Option<u32>,
    pub kind: &'static str,
}

impl LogKey {
    pub fn app(app_id: impl Into<String>, kind: &'static str) -> Self {
        Self {
            app_id: app_id.into(),
            pid: None,
            kind,
        }
    }

    pub fn pid(app_id: impl Into<String>, pid: u32, kind: &'static str) -> Self {
        Self {
            app_id: app_id.into(),
            pid: Some(pid),
            kind,
        }
    }
}

/// Decides whether a given message should be emitted now.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    window: Duration,
    seen: HashMap<LogKey, Instant>,
}

impl RateLimiter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            seen: HashMap::new(),
        }
    }

    /// Whether this message may be logged now.
    ///
    /// Returns `true` the first time a key is seen and again once the window has
    /// elapsed; suppressed calls are counted so the silence itself is visible in
    /// `app-powerd stats`.
    pub fn allow(&mut self, key: LogKey) -> bool {
        let now = Instant::now();
        self.evict(now);

        match self.seen.get(&key) {
            Some(&last) if now.duration_since(last) < self.window => {
                METRICS
                    .warns_suppressed_total
                    .fetch_add(1, Ordering::Relaxed);
                false
            }
            _ => {
                self.seen.insert(key, now);
                true
            }
        }
    }

    /// Forget a key, so a recurrence after the condition is resolved is reported
    /// immediately rather than waiting out the window.
    pub fn forget_app(&mut self, app_id: &str) {
        self.seen.retain(|key, _| key.app_id != app_id);
    }

    fn evict(&mut self, now: Instant) {
        self.seen
            .retain(|_, &mut last| now.duration_since(last) < self.window);

        if self.seen.len() <= MAX_ENTRIES {
            return;
        }
        // Still over the cap with nothing expired: drop the oldest entries. They
        // are the least likely to recur, and re-reporting one costs a single
        // line.
        let mut by_age: Vec<_> = self.seen.iter().map(|(k, &t)| (k.clone(), t)).collect();
        by_age.sort_by_key(|(_, t)| *t);
        for (key, _) in by_age.into_iter().take(self.seen.len() - MAX_ENTRIES) {
            self.seen.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_passes_and_repeat_is_suppressed() {
        let mut limiter = RateLimiter::new(Duration::from_secs(60));
        let key = LogKey::pid("Firefox", 1, "sigstop");
        assert!(limiter.allow(key.clone()));
        assert!(!limiter.allow(key));
    }

    #[test]
    fn different_keys_do_not_suppress_each_other() {
        let mut limiter = RateLimiter::new(Duration::from_secs(60));
        assert!(limiter.allow(LogKey::pid("Firefox", 1, "sigstop")));
        assert!(limiter.allow(LogKey::pid("Firefox", 2, "sigstop")));
        assert!(limiter.allow(LogKey::pid("Firefox", 1, "throttle")));
        assert!(limiter.allow(LogKey::app("Chrome", "sigstop")));
    }

    #[test]
    fn expired_entries_are_dropped_and_message_passes_again() {
        let mut limiter = RateLimiter::new(Duration::from_millis(1));
        let key = LogKey::app("Firefox", "sigstop");
        assert!(limiter.allow(key.clone()));
        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.allow(key));
        assert_eq!(limiter.seen.len(), 1);
    }

    /// The map must not grow without bound over a months-long uptime.
    #[test]
    fn entries_are_capped() {
        let mut limiter = RateLimiter::new(Duration::from_secs(3600));
        for pid in 0..(MAX_ENTRIES as u32 + 500) {
            limiter.allow(LogKey::pid("App", pid, "sigstop"));
        }
        assert!(limiter.seen.len() <= MAX_ENTRIES + 1);
    }

    #[test]
    fn forget_app_clears_only_that_app() {
        let mut limiter = RateLimiter::new(Duration::from_secs(60));
        limiter.allow(LogKey::app("Firefox", "protected"));
        limiter.allow(LogKey::app("Chrome", "protected"));
        limiter.forget_app("Firefox");
        assert!(limiter.allow(LogKey::app("Firefox", "protected")));
        assert!(!limiter.allow(LogKey::app("Chrome", "protected")));
    }
}
