use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::config::{Action, Config, PowerMode, RulesEngine};
use crate::config::loader::load_config;
use crate::config::matching::MatchContext;
use crate::desktop::window::WindowInfo;
use crate::guards::{self, GuardResult};
use crate::ipc::protocol::{AppInfo, IpcRequest, IpcResponse};
use crate::metrics::METRICS;
use crate::state::app_entry::{AppEntry, AppId};
use crate::state::machine::{AppState, TransitionAction};
use crate::state::registry::AppRegistry;
use crate::system::cgroup::CgroupManager;
use crate::system::{freeze, throttle};
use crate::system::power::PowerSource;

/// Events processed by the engine event loop.
pub enum EngineEvent {
    FocusChanged(WindowInfo),
    WindowClosed { window_id: u64 },
    SuspendTimerFired { app_id: AppId },
    MaintenanceWake { app_id: AppId },
    MaintenanceSleep { app_id: AppId },
    ConfigReloaded(Config),
    PowerSourceChanged(PowerSource),
    IpcRequest {
        request: IpcRequest,
        reply: oneshot::Sender<IpcResponse>,
    },
    Shutdown,
}

/// Main engine that coordinates all subsystems.
pub struct Engine {
    registry: AppRegistry,
    rules_engine: RulesEngine,
    cgroup_mgr: CgroupManager,
    event_rx: mpsc::Receiver<EngineEvent>,
    event_tx: mpsc::Sender<EngineEvent>,
    config_path: PathBuf,
    desktop_index: std::collections::HashMap<String, String>,
    enabled: bool,
    power_source: PowerSource,
    start_time: Instant,
}

impl Engine {
    pub fn new(config: Config, config_path: PathBuf) -> (Self, mpsc::Sender<EngineEvent>) {
        let (event_tx, event_rx) = mpsc::channel(256);

        let rules_engine = RulesEngine::new(config.clone())
            .expect("config already validated");

        let enabled = config.defaults.enabled;
        let desktop_index = crate::system::process::build_desktop_index();

        let cgroup_mgr = CgroupManager::new();
        cgroup_mgr.cleanup_stale_cgroups();

        let engine = Self {
            registry: AppRegistry::new(),
            rules_engine,
            cgroup_mgr,
            event_rx,
            event_tx: event_tx.clone(),
            config_path,
            desktop_index,
            enabled,
            power_source: PowerSource::Unknown,
            start_time: Instant::now(),
        };

        (engine, event_tx)
    }

    /// Main event loop.
    pub async fn run(mut self) {
        info!("engine started");

        while let Some(event) = self.event_rx.recv().await {
            match event {
                EngineEvent::FocusChanged(window) => self.handle_focus_changed(window),
                EngineEvent::WindowClosed { window_id } => self.handle_window_closed(window_id),
                EngineEvent::SuspendTimerFired { app_id } => self.handle_suspend_timer(app_id).await,
                EngineEvent::MaintenanceWake { app_id } => self.handle_maintenance_wake(app_id),
                EngineEvent::MaintenanceSleep { app_id } => self.handle_maintenance_sleep(app_id),
                EngineEvent::ConfigReloaded(config) => self.handle_config_reload(config),
                EngineEvent::PowerSourceChanged(source) => self.handle_power_change(source),
                EngineEvent::IpcRequest { request, reply } => {
                    let response = self.handle_ipc(request);
                    let _ = reply.send(response);
                }
                EngineEvent::Shutdown => {
                    info!("shutdown requested");
                    self.shutdown();
                    break;
                }
            }
        }

        info!("engine stopped");
    }

    fn handle_focus_changed(&mut self, window: WindowInfo) {
        METRICS.focus_changes_total.fetch_add(1, Ordering::Relaxed);

        let app_id = AppId::from_window(&window);
        debug!(app_id = %app_id, window_id = window.window_id, "focus changed");

        // Always track apps regardless of power mode.
        // Management actions (suspend timers) are gated in execute_transition.

        // Activate the focused app
        let transition = self.registry.get(&app_id).map(|entry| {
            let (new_state, action) = entry.state.on_focus_gained();
            (new_state, action, entry.state)
        });

        if let Some((new_state, action, old_state)) = transition {
            if action != TransitionAction::None {
                info!(app_id = %app_id, from = %old_state, to = %new_state, "activating");
                self.execute_transition(&app_id, new_state, action);
            }
            // Move new PID to existing cgroup if needed
            if let Some(pid) = window.pid {
                let needs_move = self.registry.get(&app_id)
                    .map(|e| !e.pids.contains(&pid))
                    .unwrap_or(false);
                if needs_move {
                    let cgroup = self.registry.get(&app_id)
                        .and_then(|e| e.cgroup_path.clone());
                    if let Some(ref path) = cgroup {
                        if let Err(e) = self.cgroup_mgr.move_pid(std::path::Path::new(path), pid) {
                            warn!(pid, error = %e, "failed to move new pid to cgroup");
                        }
                    }
                    if let Some(entry) = self.registry.get_mut(&app_id) {
                        entry.pids.push(pid);
                    }
                }
            }
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.window_info = window.clone();
                entry.add_window(window.window_id);
            }
        } else {
            // New app — register it
            let mut ctx = MatchContext::from(&window);
            // Fill desktop_file from index
            if let Some(exe) = &window.executable {
                if let Some(desktop_id) = self.desktop_index.get(exe.as_str()) {
                    ctx.desktop_file = desktop_id.clone();
                }
            }
            let policy = self.rules_engine.match_window(&ctx);
            info!(app_id = %app_id, action = ?policy.action, "new app tracked");

            let entry = AppEntry::new(app_id.clone(), window.clone(), policy);
            self.registry.insert(entry);

            // Setup cgroup if possible
            self.setup_cgroup(&app_id);
        }

        // Move all OTHER active apps to background
        let other_apps: Vec<AppId> = self
            .registry
            .iter()
            .filter(|(id, e)| **id != app_id && e.state == AppState::Active)
            .map(|(id, _)| id.clone())
            .collect();

        let transitions: Vec<_> = other_apps
            .iter()
            .filter_map(|id| {
                self.registry.get(id).map(|e| {
                    let (new_state, action) = e.state.on_focus_lost();
                    (id.clone(), new_state, action, e.state)
                })
            })
            .collect();

        for (other_id, new_state, action, old_state) in transitions {
            if action != TransitionAction::None {
                info!(app_id = %other_id, from = %old_state, to = %new_state, "backgrounding");
                self.execute_transition(&other_id, new_state, action);
            }
        }
    }

    fn handle_window_closed(&mut self, window_id: u64) {
        if let Some(entry) = self.registry.remove_window(window_id) {
            info!(app_id = %entry.app_id, "app removed (all windows closed)");

            let elapsed_ms = entry.state_since.elapsed().as_millis() as u64;

            // Restore app to normal state before removing
            match entry.state {
                AppState::Frozen => {
                    let _ = freeze::thaw_app(
                        &self.cgroup_mgr,
                        entry.cgroup_path_ref(),
                        &entry.pids,
                    );
                    METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);
                    METRICS.time_in_frozen_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
                }
                AppState::Throttled => {
                    let _ = throttle::remove_throttle(
                        &self.cgroup_mgr,
                        entry.cgroup_path_ref(),
                        &entry.pids,
                    );
                    METRICS.apps_unthrottled_total.fetch_add(1, Ordering::Relaxed);
                    METRICS.time_in_throttled_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
                }
                _ => {}
            }

            // Clean up cgroup
            if let Some(path) = entry.cgroup_path_ref() {
                let _ = self.cgroup_mgr.remove_cgroup(path);
            }
        }
    }

    async fn handle_suspend_timer(&mut self, app_id: AppId) {
        let Some(entry) = self.registry.get(&app_id) else { return };

        // Check resume grace
        if entry.in_resume_grace() {
            debug!(app_id = %app_id, "in resume grace, skipping suspend");
            return;
        }

        // Check min_suspend: app must have been in background long enough
        let min_suspend = entry.policy.min_suspend;
        let elapsed = entry.state_since.elapsed();
        if elapsed < min_suspend {
            let remaining = min_suspend - elapsed;
            debug!(app_id = %app_id, remaining_ms = remaining.as_millis(), "min_suspend not reached, rescheduling");
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                remaining,
                EngineEvent::SuspendTimerFired { app_id: app_id.clone() },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.suspend_timer = Some(handle);
            }
            return;
        }

        let should_freeze = entry.policy.action == Action::Freeze;

        // Clone data before await to release borrow on self.registry.
        // Note: PIDs may become stale during async guards check (process exits, PID reuse).
        // This is acceptable: guards may miss a check, but execute_transition re-reads fresh PIDs.
        let mut pids = entry.pids.clone();
        for &pid in &entry.pids {
            pids.extend(crate::system::process::descendant_pids(pid));
        }
        let guards_config = entry.policy.guards.clone();
        let is_fullscreen = entry.window_info.is_fullscreen;

        // Check guards before suspending (async)
        let guard_result = guards::check_guards(&pids, &guards_config, is_fullscreen, None).await;
        if guard_result != GuardResult::Allow {
            if let GuardResult::Block(reason) = guard_result {
                info!(app_id = %app_id, reason = %reason, "guard blocked suspend");
                METRICS.guard_blocks_total.fetch_add(1, Ordering::Relaxed);
            }
            // Reschedule to recheck guards later
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                std::time::Duration::from_secs(10),
                EngineEvent::SuspendTimerFired { app_id: app_id.clone() },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.suspend_timer = Some(handle);
            }
            return;
        }

        // Re-verify state after async guards check — user may have switched back
        let Some(entry) = self.registry.get(&app_id) else { return };
        if entry.state != AppState::Background {
            debug!(app_id = %app_id, state = %entry.state, "state changed during guards check, skipping suspend");
            return;
        }

        let (new_state, action) = entry.state.on_suspend_timer(should_freeze);
        if action != TransitionAction::None {
            info!(app_id = %app_id, to = %new_state, "suspend timer fired");
            self.execute_transition(&app_id, new_state, action);
        }
    }

    fn handle_maintenance_wake(&mut self, app_id: AppId) {
        let Some(entry) = self.registry.get(&app_id) else { return };
        if entry.state != AppState::Frozen {
            return;
        }

        info!(app_id = %app_id, "maintenance wake");
        let _ = freeze::thaw_app(
            &self.cgroup_mgr,
            entry.cgroup_path_ref(),
            &entry.pids,
        );
        METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);

        // Schedule re-freeze after duration
        let duration = entry.policy.maintenance_resume.duration;
        let handle = Self::spawn_delayed_event(
            &self.event_tx,
            duration,
            EngineEvent::MaintenanceSleep { app_id: app_id.clone() },
        );
        if let Some(entry) = self.registry.get_mut(&app_id) {
            entry.maintenance_timer = Some(handle);
        }
    }

    fn handle_maintenance_sleep(&mut self, app_id: AppId) {
        let Some(entry) = self.registry.get(&app_id) else { return };
        if entry.state != AppState::Frozen {
            return;
        }

        info!(app_id = %app_id, "maintenance sleep");
        let _ = freeze::freeze_app(
            &self.cgroup_mgr,
            entry.cgroup_path_ref(),
            &entry.pids,
        );
        METRICS.apps_frozen_total.fetch_add(1, Ordering::Relaxed);

        // Schedule next wake
        self.start_maintenance_timer(&app_id);
    }

    fn handle_config_reload(&mut self, config: Config) {
        match RulesEngine::new(config.clone()) {
            Ok(engine) => {
                let was_managing = self.should_manage();
                self.rules_engine = engine;
                self.enabled = config.defaults.enabled;
                METRICS.config_reloads_total.fetch_add(1, Ordering::Relaxed);
                info!("config reloaded successfully");

                if !self.should_manage() {
                    self.thaw_all();
                } else if !was_managing {
                    self.start_management();
                }
            }
            Err(e) => {
                error!(error = %e, "config reload failed");
            }
        }
    }

    fn handle_power_change(&mut self, source: PowerSource) {
        let was_managing = self.should_manage();
        info!(?source, "power source changed");
        self.power_source = source;

        if !self.should_manage() {
            self.thaw_all();
        } else if !was_managing {
            self.start_management();
        }
    }

    fn handle_ipc(&self, request: IpcRequest) -> IpcResponse {
        match request {
            IpcRequest::List => {
                let apps = self
                    .registry
                    .iter()
                    .map(|(id, entry)| AppInfo {
                        app_id: id.to_string(),
                        state: entry.state.to_string(),
                        pids: entry.pids.clone(),
                        executable: entry.window_info.executable.clone(),
                        wm_class: entry.window_info.wm_class.clone(),
                        window_title: entry.window_info.title.clone(),
                    })
                    .collect();
                IpcResponse::AppList { apps }
            }
            IpcRequest::Status => IpcResponse::Status {
                enabled: self.enabled && self.should_manage(),
                power_source: format!("{:?}", self.power_source),
                tracked_apps: self.registry.len(),
                uptime_secs: self.start_time.elapsed().as_secs(),
            },
            IpcRequest::Stats => IpcResponse::Stats {
                metrics: METRICS.snapshot(),
            },
            IpcRequest::Freeze { pid } => {
                if !is_owned_pid(pid) {
                    return IpcResponse::Error {
                        message: format!("pid {pid} is not owned by current user"),
                    };
                }
                let _ = freeze::freeze_app(&self.cgroup_mgr, None, &[pid]);
                IpcResponse::Ok {
                    message: format!("freeze signal sent to pid {pid}"),
                }
            }
            IpcRequest::Thaw { pid } => {
                if !is_owned_pid(pid) {
                    return IpcResponse::Error {
                        message: format!("pid {pid} is not owned by current user"),
                    };
                }
                let _ = freeze::thaw_app(&self.cgroup_mgr, None, &[pid]);
                IpcResponse::Ok {
                    message: format!("thaw signal sent to pid {pid}"),
                }
            }
            IpcRequest::ReloadConfig => {
                match load_config(&self.config_path) {
                    Ok(new_config) => {
                        let tx = self.event_tx.clone();
                        let _ = tx.try_send(EngineEvent::ConfigReloaded(new_config));
                        IpcResponse::Ok {
                            message: "config reload triggered".into(),
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: format!("config reload failed: {e}"),
                    },
                }
            }
            IpcRequest::Shutdown => {
                let _ = self.event_tx.try_send(EngineEvent::Shutdown);
                IpcResponse::Ok {
                    message: "shutdown scheduled".into(),
                }
            }
        }
    }

    fn execute_transition(&mut self, app_id: &AppId, new_state: AppState, action: TransitionAction) {
        // Actions that require active management: skip if management disabled
        if !self.should_manage() && action.requires_management() {
            if let Some(entry) = self.registry.get_mut(app_id) {
                entry.set_state(new_state);
            }
            return;
        }

        // Clone what we need before mutating
        let (pids, cgroup_path, policy) = {
            let Some(entry) = self.registry.get(app_id) else { return };
            (entry.pids.clone(), entry.cgroup_path.clone(), entry.policy.clone())
        };
        let cgroup_p = cgroup_path.as_deref().map(std::path::Path::new);

        match action {
            TransitionAction::StartSuspendTimer => {
                let handle = Self::spawn_delayed_event(
                    &self.event_tx,
                    policy.suspend_delay,
                    EngineEvent::SuspendTimerFired { app_id: app_id.clone() },
                );
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.cancel_suspend_timer();
                    entry.suspend_timer = Some(handle);
                }
            }
            TransitionAction::CancelSuspendTimer => {
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.cancel_suspend_timer();
                }
            }
            TransitionAction::ApplyThrottle => {
                if let Err(e) = throttle::apply_throttle(&self.cgroup_mgr, cgroup_p, &pids, &policy) {
                    warn!(app_id = %app_id, error = %e, "throttle failed");
                }
                METRICS.apps_throttled_total.fetch_add(1, Ordering::Relaxed);
            }
            TransitionAction::ApplyFreeze => {
                if let Err(e) = freeze::freeze_app(&self.cgroup_mgr, cgroup_p, &pids) {
                    warn!(app_id = %app_id, error = %e, "freeze failed");
                }
                METRICS.apps_frozen_total.fetch_add(1, Ordering::Relaxed);

                // Start maintenance timer if enabled
                if policy.maintenance_resume.enabled {
                    self.start_maintenance_timer(app_id);
                }
            }
            TransitionAction::RemoveThrottle => {
                if let Err(e) = throttle::remove_throttle(&self.cgroup_mgr, cgroup_p, &pids) {
                    warn!(app_id = %app_id, error = %e, "remove throttle failed");
                }
                METRICS.apps_unthrottled_total.fetch_add(1, Ordering::Relaxed);
                self.record_state_duration(app_id, &METRICS.time_in_throttled_ms);
            }
            TransitionAction::Thaw => {
                if let Err(e) = freeze::thaw_app(&self.cgroup_mgr, cgroup_p, &pids) {
                    warn!(app_id = %app_id, error = %e, "thaw failed");
                }
                METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);
                self.record_state_duration(app_id, &METRICS.time_in_frozen_ms);
            }
            TransitionAction::None => {}
        }

        // Update state
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_state(new_state);
        }
    }

    /// Spawn a delayed event: sleep then send the event to the engine channel.
    fn spawn_delayed_event(
        tx: &mpsc::Sender<EngineEvent>,
        delay: std::time::Duration,
        event: EngineEvent,
    ) -> tokio::task::JoinHandle<()> {
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(event).await;
        })
    }

    /// Record how long an app has been in its current state into the given metric.
    fn record_state_duration(&self, app_id: &AppId, metric: &AtomicU64) {
        if let Some(entry) = self.registry.get(app_id) {
            let elapsed_ms = entry.state_since.elapsed().as_millis() as u64;
            metric.fetch_add(elapsed_ms, Ordering::Relaxed);
        }
    }

    fn setup_cgroup(&mut self, app_id: &AppId) {
        use crate::system::cgroup::CgroupCapability;

        let pids = {
            let Some(entry) = self.registry.get(app_id) else { return };
            if entry.policy.action == Action::Ignore {
                return;
            }
            entry.pids.clone()
        };

        let result = if self.cgroup_mgr.capability == CgroupCapability::SystemdTransient {
            self.cgroup_mgr.create_cgroup_with_pids(app_id.as_str(), &pids)
        } else {
            self.cgroup_mgr.create_cgroup(app_id.as_str())
        };

        match result {
            Ok(path) => {
                if self.cgroup_mgr.capability != CgroupCapability::SystemdTransient {
                    for &pid in &pids {
                        if let Err(e) = self.cgroup_mgr.move_pid(&path, pid) {
                            warn!(pid, error = %e, "failed to move pid to cgroup");
                        }
                    }
                }
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.cgroup_path = Some(path.to_string_lossy().to_string());
                }
            }
            Err(e) => {
                debug!(app_id = %app_id, error = %e, "cgroup setup failed, will use signal fallback");
            }
        }
    }

    fn start_maintenance_timer(&mut self, app_id: &AppId) {
        let Some(entry) = self.registry.get_mut(app_id) else { return };

        entry.cancel_maintenance_timer();
        let interval = entry.policy.maintenance_resume.interval;

        entry.maintenance_timer = Some(Self::spawn_delayed_event(
            &self.event_tx,
            interval,
            EngineEvent::MaintenanceWake { app_id: app_id.clone() },
        ));
    }

    fn start_management(&mut self) {
        info!("management activated, starting suspend timers for background apps");
        let background_apps: Vec<(AppId, std::time::Duration)> = self
            .registry
            .iter()
            .filter(|(_, e)| e.state == AppState::Background && e.policy.action != Action::Ignore)
            .map(|(id, e)| (id.clone(), e.policy.suspend_delay))
            .collect();

        for (app_id, delay) in background_apps {
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                delay,
                EngineEvent::SuspendTimerFired { app_id: app_id.clone() },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.cancel_suspend_timer();
                entry.suspend_timer = Some(handle);
            }
        }
    }

    fn should_manage(&self) -> bool {
        if !self.enabled {
            return false;
        }

        let config = self.rules_engine.config();
        match self.power_source {
            PowerSource::Ac => config.defaults.mode.ac == PowerMode::Enable,
            PowerSource::Battery => config.defaults.mode.battery == PowerMode::Enable,
            PowerSource::Unknown => true,
        }
    }

    fn thaw_all(&mut self) {
        let app_ids: Vec<AppId> = self
            .registry
            .iter()
            .filter(|(_, e)| matches!(e.state, AppState::Frozen | AppState::Throttled))
            .map(|(id, _)| id.clone())
            .collect();

        let transitions: Vec<_> = app_ids
            .iter()
            .filter_map(|id| {
                self.registry.get(id).map(|e| {
                    let (new_state, action) = e.state.on_focus_gained();
                    (id.clone(), new_state, action)
                })
            })
            .collect();

        for (app_id, new_state, action) in transitions {
            self.execute_transition(&app_id, new_state, action);
        }
    }

    fn shutdown(&mut self) {
        info!("graceful shutdown: thawing all apps");
        self.thaw_all();

        // Cancel all timers and clean up cgroups
        for (_, entry) in self.registry.iter_mut() {
            entry.cancel_all_timers();
            if let Some(path) = entry.cgroup_path_ref() {
                let _ = self.cgroup_mgr.remove_cgroup(path);
            }
        }
    }
}

/// Check if the given PID belongs to the current user.
fn is_owned_pid(pid: u32) -> bool {
    let metadata = match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(m) => m,
        Err(_) => return false,
    };
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == nix::unistd::getuid().as_raw()
}
