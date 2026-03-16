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
use crate::ipc::protocol::{AppInfo, IpcRequest, IpcResponse};
use crate::metrics::METRICS;
use crate::state::{AppEntry, AppId, AppRegistry, AppState, SuspendMode, TransitionAction};
use crate::system::cgroup::CgroupManager;
use crate::system::power::PowerSource;
use crate::system::{freeze, throttle};

mod config_power;
mod focus;
mod ipc_handler;
mod suspend;
mod transitions;

/// Channel capacity for engine events.
const ENGINE_CHANNEL_CAPACITY: usize = 256;

/// Interval to retry suspend after a guard block or transient failure.
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for collecting descendant PIDs via /proc scan.
const DESCENDANT_PIDS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Events processed by the engine event loop.
#[non_exhaustive]
pub enum EngineEvent {
    FocusChanged(WindowInfo),
    WindowClosed {
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
    Shutdown,
}

impl From<crate::desktop::FocusEvent> for EngineEvent {
    fn from(event: crate::desktop::FocusEvent) -> Self {
        match event {
            crate::desktop::FocusEvent::FocusChanged(w) => EngineEvent::FocusChanged(w),
            crate::desktop::FocusEvent::WindowClosed { window_id } => {
                EngineEvent::WindowClosed { window_id }
            }
        }
    }
}

/// Main engine that coordinates all subsystems.
pub struct Engine {
    registry: AppRegistry,
    rules_engine: RulesEngine,
    cgroup_mgr: CgroupManager,
    event_rx: mpsc::Receiver<EngineEvent>,
    event_tx: mpsc::Sender<EngineEvent>,
    config_path: PathBuf,
    exe_to_desktop: std::collections::HashMap<String, String>,
    enabled: bool,
    power_source: PowerSource,
    start_time: Instant,
}

impl Engine {
    pub fn new(
        config: Config,
        config_path: PathBuf,
    ) -> Result<(Self, mpsc::Sender<EngineEvent>), crate::error::ConfigError> {
        let (event_tx, event_rx) = mpsc::channel(ENGINE_CHANNEL_CAPACITY);

        let rules_engine = RulesEngine::new(config.clone())?;

        let enabled = config.defaults.enabled;
        let exe_to_desktop = crate::system::process::build_desktop_index();

        let cgroup_mgr = CgroupManager::new();
        cgroup_mgr.cleanup_stale_cgroups();

        let engine = Self {
            registry: AppRegistry::new(),
            rules_engine,
            cgroup_mgr,
            event_rx,
            event_tx: event_tx.clone(),
            config_path,
            exe_to_desktop,
            enabled,
            power_source: PowerSource::Unknown,
            start_time: Instant::now(),
        };

        Ok((engine, event_tx))
    }

    /// Main event loop.
    #[instrument(name = "engine", skip_all)]
    pub async fn run(mut self) {
        info!("engine started");

        while let Some(event) = self.event_rx.recv().await {
            match event {
                EngineEvent::FocusChanged(window) => self.handle_focus_changed(window),
                EngineEvent::WindowClosed { window_id } => self.handle_window_closed(window_id),
                EngineEvent::SuspendTimerFired { app_id } => {
                    self.handle_suspend_timer(app_id).await
                }
                EngineEvent::MaintenanceWake { app_id } => self.handle_maintenance_wake(app_id),
                EngineEvent::MaintenanceSleep { app_id } => self.handle_maintenance_sleep(app_id),
                EngineEvent::ConfigReloaded(config) => self.handle_config_reload(config),
                EngineEvent::PowerSourceChanged(source) => self.handle_power_change(source),
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
