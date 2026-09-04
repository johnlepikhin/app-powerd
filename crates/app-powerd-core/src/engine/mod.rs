use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, instrument, warn};

use crate::config::loader::load_config;
use crate::config::matching::MatchContext;
use crate::config::{Action, Config, PowerMode, RulesEngine};
use crate::desktop::window::WindowInfo;
use crate::guards::{self, GuardResult};
use crate::ipc::protocol::{AppInfo, IpcRequest, IpcResponse, Target};
use crate::metrics::METRICS;
use crate::state::journal::{FreezeJournal, FreezeMethod};
use crate::state::{AppEntry, AppId, AppRegistry, AppState, SuspendMode, TransitionAction};
use crate::system::cgroup::CgroupManager;
use crate::system::power::PowerSource;
use crate::system::protection::{ProtectionPolicy, ProtectionReason};
use crate::system::ProcessHandle;
use crate::system::{freeze, throttle};

use rate_limit::{LogKey, RateLimiter};

mod config_power;
mod focus;
mod ipc_handler;
mod rate_limit;
mod reconcile;
mod suspend;
mod transitions;

/// Channel capacity for engine events.
const ENGINE_CHANNEL_CAPACITY: usize = 256;

/// Interval to retry suspend after a guard block or transient failure.
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Upper bound on the retry backoff after repeated failures.
const MAX_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Give up retrying a suspend after this many consecutive failures.
///
/// Without a limit a permanently failing target — a process owned by another
/// user, say — is retried forever, which is how the original defect turned a
/// single unreachable PID into an endless stream of log lines.
const MAX_SUSPEND_ATTEMPTS: u32 = 5;

/// Timeout for collecting descendant PIDs via /proc scan.
const DESCENDANT_PIDS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Time budget for one sweep of the session bus.
const DBUS_SWEEP_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// How long an identical warning is suppressed after being emitted once.
const LOG_SUPPRESSION_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

/// Events processed by the engine event loop.
#[non_exhaustive]
pub enum EngineEvent {
    FocusChanged(WindowInfo),
    WindowClosed {
        window_id: u64,
    },
    /// User switched to a different virtual desktop (EWMH `_NET_CURRENT_DESKTOP`).
    /// Engine pre-thaws any frozen tracked app whose window lives on this desktop
    /// so the user can interact with it (open from tray, click) without waiting
    /// for the next maintenance window.
    WorkspaceChanged {
        desktop: u32,
    },
    /// A panel/launcher requested activation of `window_id` via
    /// `_NET_ACTIVE_WINDOW` ClientMessage. Engine pre-thaws the matching app
    /// so it can actually respond before the WM hands the request over.
    ActivationRequested {
        window_id: u64,
    },
    SuspendTimerFired {
        app_id: AppId,
    },
    MaintenanceWake {
        app_id: AppId,
    },
    MaintenanceSleep {
        app_id: AppId,
    },
    ConfigReloaded(Config),
    PowerSourceChanged(PowerSource),
    IpcRequest {
        request: IpcRequest,
        reply: oneshot::Sender<IpcResponse>,
    },
    /// Re-check that tracked processes still exist and retire what is gone.
    ///
    /// Normally raised by the engine's own interval; also accepted from outside
    /// so tests can drive a sweep deterministically instead of waiting.
    Reconcile,
    /// Result of a blocking reconcile sweep.
    ///
    /// Always delivered once per sweep, including when nothing was found, so the
    /// in-flight flag is cleared even if the blocking task failed.
    ReconcileResult {
        /// Per application, the root processes found to have exited.
        reaped: Vec<(AppId, Vec<ProcessHandle>)>,
        /// Per journal key, the recorded PIDs found to have exited.
        journal_dead: Vec<(String, Vec<u32>)>,
    },
    /// A refreshed set of session-bus name owners, collected off the event loop.
    ProtectionRefreshed(std::collections::HashMap<u32, String>),
    Shutdown,
}

impl From<crate::desktop::FocusEvent> for EngineEvent {
    fn from(event: crate::desktop::FocusEvent) -> Self {
        match event {
            crate::desktop::FocusEvent::FocusChanged(w) => EngineEvent::FocusChanged(w),
            crate::desktop::FocusEvent::WindowClosed { window_id } => {
                EngineEvent::WindowClosed { window_id }
            }
            crate::desktop::FocusEvent::WorkspaceChanged { desktop } => {
                EngineEvent::WorkspaceChanged { desktop }
            }
            crate::desktop::FocusEvent::ActivationRequested { window_id } => {
                EngineEvent::ActivationRequested { window_id }
            }
        }
    }
}

/// Main engine that coordinates all subsystems.
pub struct Engine {
    registry: AppRegistry,
    rules_engine: RulesEngine,
    cgroup_mgr: CgroupManager,
    /// Durable record of what is suspended, so an abrupt exit is recoverable.
    journal: FreezeJournal,
    /// Processes that must never be suspended.
    ///
    /// Cloned into blocking tasks that classify a process tree, so there is one
    /// authoritative copy here and no mirrored owner set to drift out of sync.
    protection: ProtectionPolicy,
    /// Suppression of repeated warnings, and of the "protected" notice which is
    /// reported once per application rather than once per attempt.
    rate_limiter: RateLimiter,
    /// Consecutive failed suspend attempts, for backoff.
    suspend_failures: std::collections::HashMap<AppId, u32>,
    event_rx: mpsc::Receiver<EngineEvent>,
    event_tx: mpsc::Sender<EngineEvent>,
    config_path: PathBuf,
    exe_to_desktop: std::collections::HashMap<String, String>,
    enabled: bool,
    power_source: PowerSource,
    forced_power_source: Option<PowerSource>,
    start_time: Instant,
    reconcile_interval: std::time::Duration,
    dbus_check: bool,
    dbus_refresh_interval: std::time::Duration,
    last_dbus_refresh: Option<Instant>,
    /// Whether a reconcile sweep is still running on the blocking pool.
    ///
    /// The tick interval bounds how often a sweep starts, not how long one
    /// takes; without this, a slow `/proc` lets sweeps pile up, each holding a
    /// thread of the blocking pool.
    reconcile_in_flight: bool,
}

impl Engine {
    /// Construct an engine **without** a freeze journal.
    ///
    /// Nothing this engine suspends is recorded on disk, so processes stopped by
    /// it cannot be released after an abrupt exit — neither by a later run of the
    /// daemon nor by anything else, since `SIGSTOP` is not reversible from the
    /// stopped process. Use it only where that is acceptable (tests, an embedder
    /// that never freezes); a daemon that suspends real applications must use
    /// [`Engine::with_journal`] with a journal loaded from
    /// [`FreezeJournal::default_path`].
    pub fn new(
        config: Config,
        config_path: PathBuf,
    ) -> Result<(Self, mpsc::Sender<EngineEvent>), crate::error::ConfigError> {
        Self::with_journal(config, config_path, FreezeJournal::disabled())
    }

    /// Construct an engine backed by a specific journal.
    ///
    /// Separate from [`Engine::new`] so tests can run without touching the real
    /// runtime directory, and so daemon startup can hand over a journal it has
    /// already recovered from.
    pub fn with_journal(
        config: Config,
        config_path: PathBuf,
        journal: FreezeJournal,
    ) -> Result<(Self, mpsc::Sender<EngineEvent>), crate::error::ConfigError> {
        let (event_tx, event_rx) = mpsc::channel(ENGINE_CHANNEL_CAPACITY);

        let rules_engine = RulesEngine::new(config.clone())?;

        let enabled = config.defaults.enabled;
        let exe_to_desktop = crate::system::process::build_desktop_index();

        let cgroup_mgr = CgroupManager::new();
        cgroup_mgr.report_capabilities(&config.profiles_requiring_cpu_control());
        cgroup_mgr.cleanup_stale_cgroups();

        let engine = Self {
            registry: AppRegistry::new(),
            rules_engine,
            cgroup_mgr,
            journal,
            protection: ProtectionPolicy::default(),
            rate_limiter: RateLimiter::new(LOG_SUPPRESSION_WINDOW),
            suspend_failures: std::collections::HashMap::new(),
            event_rx,
            event_tx: event_tx.clone(),
            config_path,
            exe_to_desktop,
            enabled,
            power_source: PowerSource::Unknown,
            forced_power_source: None,
            start_time: Instant::now(),
            reconcile_interval: config.defaults.reconcile_interval,
            dbus_check: config.defaults.protection.dbus_check,
            dbus_refresh_interval: config.defaults.protection.dbus_refresh_interval,
            last_dbus_refresh: None,
            reconcile_in_flight: false,
        };

        Ok((engine, event_tx))
    }

    /// Main event loop.
    ///
    /// The reconcile interval lives here rather than in the binary so that any
    /// embedder of this crate — and every integration test — gets the same
    /// self-correcting behaviour. Driving it from `main` would leave `Engine`
    /// accumulating dead PIDs by default and let tests exercise a code path the
    /// daemon never takes.
    #[instrument(name = "engine", skip_all)]
    pub async fn run(mut self) {
        info!("engine started");

        let mut reconcile = tokio::time::interval(self.reconcile_interval);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick completes immediately; skip it so startup is not
        // interrupted by a sweep over an empty registry.
        reconcile.tick().await;

        loop {
            let event = tokio::select! {
                maybe_event = self.event_rx.recv() => match maybe_event {
                    Some(event) => event,
                    None => break,
                },
                _ = reconcile.tick() => EngineEvent::Reconcile,
            };

            match event {
                EngineEvent::FocusChanged(window) => self.handle_focus_changed(window),
                EngineEvent::WindowClosed { window_id } => self.handle_window_closed(window_id),
                EngineEvent::WorkspaceChanged { desktop } => self.handle_workspace_changed(desktop),
                EngineEvent::ActivationRequested { window_id } => {
                    self.handle_activation_requested(window_id)
                }
                EngineEvent::SuspendTimerFired { app_id } => {
                    self.handle_suspend_timer(app_id).await
                }
                EngineEvent::MaintenanceWake { app_id } => self.handle_maintenance_wake(app_id),
                EngineEvent::MaintenanceSleep { app_id } => self.handle_maintenance_sleep(app_id),
                EngineEvent::ConfigReloaded(config) => self.handle_config_reload(config),
                EngineEvent::PowerSourceChanged(source) => self.handle_power_change(source),
                EngineEvent::Reconcile => self.handle_reconcile(),
                EngineEvent::ReconcileResult {
                    reaped,
                    journal_dead,
                } => self.apply_reconcile_result(reaped, journal_dead),
                EngineEvent::ProtectionRefreshed(owners) => self.protection.apply_refresh(owners),
                EngineEvent::IpcRequest { request, reply } => {
                    let response = self.handle_ipc(request);
                    if reply.send(response).is_err() {
                        warn!("IPC reply channel closed, client disconnected");
                    }
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
}
