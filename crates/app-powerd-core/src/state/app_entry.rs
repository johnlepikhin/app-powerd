use std::path::Path;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::machine::AppState;
use crate::config::ResolvedPolicy;
use crate::desktop::window::WindowInfo;

/// Unique identifier for a tracked application.
/// Based on executable name (groups multiple windows of same app).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppId(String);

impl AppId {
    /// Derive AppId from window info. Uses wm_class > app_id > executable > window_id.
    pub fn from_window(info: &WindowInfo) -> Self {
        let id = info
            .wm_class
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| info.app_id.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| info.executable.as_deref().filter(|s| !s.is_empty()))
            .map(String::from)
            .unwrap_or_else(|| format!("window-{}", info.window_id));
        AppId(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Tracked application entry in the registry.
pub struct AppEntry {
    pub(crate) app_id: AppId,
    pub(crate) state: AppState,
    pub(crate) pids: Vec<u32>,
    pub(crate) window_ids: Vec<u64>,
    pub(crate) window_info: WindowInfo,
    pub(crate) policy: ResolvedPolicy,
    pub(crate) cgroup_path: Option<std::path::PathBuf>,

    /// When the app entered its current state.
    pub(crate) state_since: Instant,
    /// When the app was last active (for resume_grace).
    pub(crate) last_active: Instant,

    /// Handle for the suspend_delay timer task (can be aborted).
    pub(crate) suspend_timer: Option<JoinHandle<()>>,
    /// Handle for the maintenance resume timer task.
    pub(crate) maintenance_timer: Option<JoinHandle<()>>,
}

impl AppEntry {
    pub fn new(app_id: AppId, window_info: WindowInfo, policy: ResolvedPolicy) -> Self {
        let now = Instant::now();
        let pids = window_info.pid.into_iter().collect();

        Self {
            app_id,
            state: AppState::Active,
            pids,
            window_ids: vec![window_info.window_id],
            window_info,
            policy,
            cgroup_path: None,
            state_since: now,
            last_active: now,
            suspend_timer: None,
            maintenance_timer: None,
        }
    }

    // --- Read-only accessors ---

    pub fn app_id(&self) -> &AppId {
        &self.app_id
    }

    pub fn state(&self) -> AppState {
        self.state
    }

    pub fn state_since(&self) -> Instant {
        self.state_since
    }

    pub fn pids(&self) -> &[u32] {
        &self.pids
    }

    pub fn policy(&self) -> &ResolvedPolicy {
        &self.policy
    }

    pub fn window_info(&self) -> &WindowInfo {
        &self.window_info
    }

    pub fn window_ids(&self) -> &[u64] {
        &self.window_ids
    }

    pub fn cgroup_path_buf(&self) -> Option<std::path::PathBuf> {
        self.cgroup_path.clone()
    }

    // --- Mutating methods ---

    pub fn add_pid(&mut self, pid: u32) {
        if !self.pids.contains(&pid) {
            self.pids.push(pid);
        }
    }

    pub fn contains_pid(&self, pid: u32) -> bool {
        self.pids.contains(&pid)
    }

    pub fn update_window_info(&mut self, info: WindowInfo) {
        self.window_info = info;
    }

    pub fn set_suspend_timer(&mut self, handle: JoinHandle<()>) {
        self.cancel_suspend_timer();
        self.suspend_timer = Some(handle);
    }

    pub fn set_maintenance_timer(&mut self, handle: JoinHandle<()>) {
        self.cancel_maintenance_timer();
        self.maintenance_timer = Some(handle);
    }

    pub fn set_cgroup_path(&mut self, path: std::path::PathBuf) {
        self.cgroup_path = Some(path);
    }

    pub fn set_policy(&mut self, policy: ResolvedPolicy) {
        self.policy = policy;
    }

    /// Update state and record timestamp.
    /// Reset `state_since` to now (for maintenance wake/sleep duration tracking).
    pub fn reset_state_since(&mut self) {
        self.state_since = Instant::now();
    }

    pub fn set_state(&mut self, new_state: AppState) {
        let now = Instant::now();
        if new_state == AppState::Active {
            self.last_active = now;
        }
        self.state = new_state;
        self.state_since = now;
    }

    /// Cancel any pending suspend timer.
    pub fn cancel_suspend_timer(&mut self) {
        if let Some(handle) = self.suspend_timer.take() {
            handle.abort();
        }
    }

    /// Cancel maintenance timer.
    pub fn cancel_maintenance_timer(&mut self) {
        if let Some(handle) = self.maintenance_timer.take() {
            handle.abort();
        }
    }

    /// Cancel all timers.
    pub fn cancel_all_timers(&mut self) {
        self.cancel_suspend_timer();
        self.cancel_maintenance_timer();
    }

    /// Get the cgroup path as a `&Path`, if set.
    pub fn cgroup_path_ref(&self) -> Option<&Path> {
        self.cgroup_path.as_deref()
    }

    /// Check if resume_grace period has not expired yet.
    pub fn in_resume_grace(&self) -> bool {
        self.last_active.elapsed() < self.policy.resume_grace
    }

    /// Whether this app has a given window.
    pub fn has_window(&self, window_id: u64) -> bool {
        self.window_ids.contains(&window_id)
    }

    /// Add a window to this app.
    pub fn add_window(&mut self, window_id: u64) {
        if !self.window_ids.contains(&window_id) {
            self.window_ids.push(window_id);
        }
    }

    /// Remove a window. Returns true if no windows remain.
    pub fn remove_window(&mut self, window_id: u64) -> bool {
        self.window_ids.retain(|&id| id != window_id);
        self.window_ids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedPolicy;
    use crate::desktop::window::WindowInfo;

    #[test]
    fn app_id_from_window_prefers_wm_class() {
        let mut info = WindowInfo::new(1);
        info.wm_class = Some("Firefox".into());
        info.app_id = Some("org.mozilla.Firefox".into());
        info.executable = Some("firefox".into());
        assert_eq!(AppId::from_window(&info).as_str(), "Firefox");
    }

    #[test]
    fn app_id_from_window_falls_back_to_app_id() {
        let mut info = WindowInfo::new(1);
        info.app_id = Some("org.mozilla.Firefox".into());
        info.executable = Some("firefox".into());
        assert_eq!(AppId::from_window(&info).as_str(), "org.mozilla.Firefox");
    }

    #[test]
    fn app_id_from_window_falls_back_to_executable() {
        let mut info = WindowInfo::new(1);
        info.executable = Some("firefox".into());
        assert_eq!(AppId::from_window(&info).as_str(), "firefox");
    }

    #[test]
    fn app_id_from_window_falls_back_to_window_id() {
        let info = WindowInfo::new(42);
        assert_eq!(AppId::from_window(&info).as_str(), "window-42");
    }

    #[test]
    fn add_window_dedup() {
        let info = WindowInfo::new(1);
        let mut entry = AppEntry::new(AppId::from_window(&info), info, ResolvedPolicy::default());
        entry.add_window(1); // duplicate
        entry.add_window(2);
        assert_eq!(entry.window_ids().len(), 2);
    }

    #[test]
    fn remove_window_returns_empty() {
        let info = WindowInfo::new(1);
        let mut entry = AppEntry::new(AppId::from_window(&info), info, ResolvedPolicy::default());
        entry.add_window(2);
        assert!(!entry.remove_window(1)); // still has window 2
        assert!(entry.remove_window(2)); // now empty
    }
}
