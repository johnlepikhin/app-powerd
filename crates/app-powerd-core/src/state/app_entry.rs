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
            .or(info.app_id.as_deref())
            .or(info.executable.as_deref())
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
    pub app_id: AppId,
    pub state: AppState,
    pub pids: Vec<u32>,
    pub window_ids: Vec<u64>,
    pub window_info: WindowInfo,
    pub policy: ResolvedPolicy,
    pub cgroup_path: Option<String>,

    /// When the app entered its current state.
    pub state_since: Instant,
    /// When the app was last active (for resume_grace).
    pub last_active: Instant,

    /// Handle for the suspend_delay timer task (can be aborted).
    pub suspend_timer: Option<JoinHandle<()>>,
    /// Handle for the maintenance resume timer task.
    pub maintenance_timer: Option<JoinHandle<()>>,
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

    /// Update state and record timestamp.
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
        self.cgroup_path.as_deref().map(Path::new)
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
