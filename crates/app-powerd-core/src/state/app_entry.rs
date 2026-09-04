use std::path::Path;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::machine::AppState;
use super::process_set::ProcessSet;
use crate::config::ResolvedPolicy;
use crate::desktop::window::WindowInfo;

/// Unique identifier for a tracked application.
///
/// Derived from the window (wm_class > app_id > executable > window_id), which
/// means the same program can present itself under different capitalisations —
/// `zenity` reports `WM_CLASS` "Zenity" but an executable "zenity". Identity is
/// therefore held case-insensitively in `key`, while `display` preserves the
/// spelling the desktop actually used so output stays recognisable.
///
/// Case folding is ASCII-only, deliberately: rule matching
/// (`config::matching`) and the protection deny-list both compare with
/// `eq_ignore_ascii_case`. Folding identity over full Unicode here would make
/// two names the same application while no rule written for either of them
/// matches both.
#[derive(Debug, Clone)]
pub struct AppId {
    key: String,
    display: String,
}

impl AppId {
    /// Derive AppId from window info. Uses wm_class > app_id > executable > window_id.
    pub fn from_window(info: &WindowInfo) -> Self {
        let display = info
            .wm_class
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| info.app_id.as_deref().filter(|s| !s.is_empty()))
            .or_else(|| info.executable.as_deref().filter(|s| !s.is_empty()))
            .map(String::from)
            .unwrap_or_else(|| format!("window-{}", info.window_id));
        Self::new(display)
    }

    /// Build an AppId from an explicit name, e.g. one supplied over IPC.
    pub fn new(display: impl Into<String>) -> Self {
        let display = display.into();
        Self {
            key: display.to_ascii_lowercase(),
            display,
        }
    }

    /// The name as the desktop spelled it — for logs and CLI output.
    pub fn as_str(&self) -> &str {
        &self.display
    }

    /// The case-folded identity used for lookup and comparison.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl PartialEq for AppId {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for AppId {}

impl std::hash::Hash for AppId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

impl std::fmt::Display for AppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display)
    }
}

/// Tracked application entry in the registry.
pub struct AppEntry {
    pub(crate) app_id: AppId,
    pub(crate) state: AppState,
    /// Processes that own this application's windows.
    ///
    /// Deliberately *not* the full descendant tree. These identify the
    /// application and are cheap to re-check every reconcile tick; the far
    /// larger set of processes actually signalled is recorded in the freeze
    /// journal instead, where it outlives this entry and can still be thawed
    /// after the main process dies.
    pub(crate) roots: ProcessSet,
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
        let mut roots = ProcessSet::default();
        if let Some(pid) = window_info.pid {
            // A window whose PID cannot be identified still deserves tracking —
            // we follow its state and its rules — it simply owns no process we
            // are willing to signal.
            roots.insert_pid(pid);
        }

        Self {
            app_id,
            state: AppState::Active,
            roots,
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

    /// PIDs of the window-owning processes.
    pub fn pids(&self) -> Vec<u32> {
        self.roots.pids()
    }

    /// Identities of the window-owning processes.
    pub fn roots(&self) -> &ProcessSet {
        &self.roots
    }

    pub fn roots_mut(&mut self) -> &mut ProcessSet {
        &mut self.roots
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

    /// Record a newly seen window-owning PID, capturing its identity.
    ///
    /// Returns the captured handle, or `None` if the process could not be
    /// identified (already exited, or `/proc` unreadable).
    pub fn add_pid(&mut self, pid: u32) -> Option<crate::system::ProcessHandle> {
        self.roots.insert_pid(pid)
    }

    pub fn contains_pid(&self, pid: u32) -> bool {
        self.roots.contains_pid(pid)
    }

    /// Whether every process this application ever owned has exited.
    ///
    /// False for an entry that never learned a PID: that is an identification
    /// gap, not a dead application, and removing it would lose a window we are
    /// still tracking.
    pub fn is_defunct(&self) -> bool {
        self.roots.is_empty() && self.roots.had_live_procs()
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
