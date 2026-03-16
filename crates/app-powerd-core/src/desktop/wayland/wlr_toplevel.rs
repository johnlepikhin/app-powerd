use std::collections::HashMap;
use std::os::fd::AsRawFd;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::info;
use wayland_client::protocol::{wl_registry, wl_registry::WlRegistry};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::desktop::window::WindowInfo;
use crate::desktop::{FocusBackend, FocusEvent};
use crate::error::DesktopError;

/// State for each tracked toplevel.
#[derive(Debug, Clone, Default)]
struct ToplevelState {
    title: String,
    app_id: String,
    is_activated: bool,
    is_fullscreen: bool,
}

/// Cached PID info to avoid repeated /proc scans.
struct CachedPid {
    pid: u32,
    info: crate::system::process::CachedProcessInfo,
}

/// Shared state between dispatch callbacks and the async loop.
struct WlrState {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<u64, ToplevelState>,
    /// Pending state updates (accumulated before `done` event).
    pending: HashMap<u64, ToplevelState>,
    /// Events to be sent to the engine.
    pending_events: Vec<FocusEvent>,
    /// Counter for generating unique toplevel IDs.
    next_id: u64,
    /// Maps wayland object ID to our internal ID.
    handle_ids: HashMap<u32, u64>,
    /// Cache: app_id → PID info (avoids /proc scan on every focus change).
    pid_cache: HashMap<String, CachedPid>,
}

impl WlrState {
    fn new() -> Self {
        Self {
            manager: None,
            toplevels: HashMap::new(),
            pending: HashMap::new(),
            pending_events: Vec::new(),
            next_id: 1,
            handle_ids: HashMap::new(),
            pid_cache: HashMap::new(),
        }
    }

    fn get_or_create_id(&mut self, handle: &ZwlrForeignToplevelHandleV1) -> u64 {
        let wl_id = wayland_client::Proxy::id(handle).protocol_id();
        *self.handle_ids.entry(wl_id).or_insert_with(|| {
            let id = self.next_id;
            self.next_id += 1;
            id
        })
    }
}

impl Dispatch<WlRegistry, ()> for WlrState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "zwlr_foreign_toplevel_manager_v1" {
                let manager = registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(
                    name,
                    version.min(3),
                    qh,
                    (),
                );
                state.manager = Some(manager);
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Finished = event {
            info!("wlr-foreign-toplevel-manager finished");
        }
    }

    event_created_child!(WlrState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let id = state.get_or_create_id(handle);

        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.pending.entry(id).or_default().title = title;
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.pending.entry(id).or_default().app_id = app_id;
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: raw_state } => {
                let pending = state.pending.entry(id).or_default();
                // State is a list of u32 values
                let states: Vec<u32> = raw_state
                    .chunks_exact(4)
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                pending.is_activated =
                    states.contains(&(zwlr_foreign_toplevel_handle_v1::State::Activated as u32));
                pending.is_fullscreen =
                    states.contains(&(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32));
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if let Some(pending) = state.pending.remove(&id) {
                    let prev = state.toplevels.get(&id);
                    let was_activated = prev.map(|s| s.is_activated).unwrap_or(false);
                    let was_fullscreen = prev.map(|s| s.is_fullscreen).unwrap_or(false);

                    state.toplevels.insert(id, pending.clone());

                    if pending.is_activated
                        && (!was_activated || pending.is_fullscreen != was_fullscreen)
                    {
                        // Focus changed or fullscreen state changed on active toplevel
                        let info = toplevel_to_window_info(id, &pending, &mut state.pid_cache);
                        state.pending_events.push(FocusEvent::FocusChanged(info));
                    }
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                let wl_id = wayland_client::Proxy::id(handle).protocol_id();
                state.handle_ids.remove(&wl_id);
                if let Some(toplevel) = state.toplevels.remove(&id) {
                    state.pid_cache.remove(&toplevel.app_id);
                }
                state.pending.remove(&id);
                state
                    .pending_events
                    .push(FocusEvent::WindowClosed { window_id: id });
            }
            _ => {}
        }
    }
}

fn toplevel_to_window_info(
    id: u64,
    toplevel: &ToplevelState,
    pid_cache: &mut HashMap<String, CachedPid>,
) -> WindowInfo {
    let mut info = WindowInfo::new(id);
    info.title = Some(toplevel.title.clone());
    info.app_id = Some(toplevel.app_id.clone());
    info.wm_class = Some(toplevel.app_id.clone());
    info.is_fullscreen = toplevel.is_fullscreen;

    // wlr-foreign-toplevel doesn't provide PID. Use cache or scan /proc.
    if !toplevel.app_id.is_empty() {
        if let Some(cached) = pid_cache.get(&toplevel.app_id) {
            info.pid = Some(cached.pid);
            info.executable = Some(cached.info.exe.clone());
            info.cmdline = cached.info.cmdline.clone();
        } else if let Some(pid) = find_pid_by_app_id(&toplevel.app_id) {
            let exe = crate::system::process::exe_name(pid).unwrap_or_default();
            let cmdline = crate::system::process::cmdline(pid).ok();
            info.pid = Some(pid);
            info.executable = Some(exe.clone());
            info.cmdline = cmdline.clone();
            pid_cache.insert(
                toplevel.app_id.clone(),
                CachedPid {
                    pid,
                    info: crate::system::process::CachedProcessInfo { exe, cmdline },
                },
            );
        }
    }

    info
}

/// Best-effort PID lookup: scan /proc for a process whose exe matches app_id.
fn find_pid_by_app_id(app_id: &str) -> Option<u32> {
    let app_id_lower = app_id.to_lowercase();
    let proc_dir = std::fs::read_dir("/proc").ok()?;

    for entry in proc_dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(exe) = crate::system::process::exe_name(pid) {
            if exe.to_lowercase() == app_id_lower {
                return Some(pid);
            }
        }
    }
    None
}

pub struct WlrToplevelBackend {
    conn: Connection,
}

impl WlrToplevelBackend {
    pub fn new() -> Result<Self, DesktopError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| DesktopError::WaylandConnection(format!("Wayland connect: {e}")))?;

        let mut event_queue = conn.new_event_queue::<WlrState>();
        let qh = event_queue.handle();
        let display = conn.display();
        display.get_registry(&qh, ());

        let mut state = WlrState::new();

        // Roundtrip to bind the manager
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| DesktopError::WaylandConnection(format!("roundtrip: {e}")))?;

        if state.manager.is_none() {
            return Err(DesktopError::WaylandConnection(
                "zwlr_foreign_toplevel_manager_v1 not available".into(),
            ));
        }

        Ok(Self { conn })
    }
}

#[async_trait::async_trait]
impl FocusBackend for WlrToplevelBackend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        let conn = self.conn;

        let mut event_queue: EventQueue<WlrState> = conn.new_event_queue();
        let qh = event_queue.handle();
        let display = conn.display();
        display.get_registry(&qh, ());

        let mut state = WlrState::new();

        event_queue
            .roundtrip(&mut state)
            .map_err(|e| DesktopError::WaylandConnection(format!("roundtrip: {e}")))?;

        // Second roundtrip to get initial toplevel list
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| DesktopError::WaylandConnection(format!("roundtrip: {e}")))?;

        // Send initial events
        for event in state.pending_events.drain(..) {
            if tx.send(event).await.is_err() {
                return Ok(());
            }
        }

        let fd = conn
            .prepare_read()
            .map(|g| g.connection_fd().as_raw_fd())
            .ok_or_else(|| DesktopError::WaylandConnection("cannot get fd".into()))?;
        let async_fd = AsyncFd::new(fd)
            .map_err(|e| DesktopError::WaylandConnection(format!("AsyncFd: {e}")))?;

        loop {
            let mut guard = async_fd
                .readable()
                .await
                .map_err(|e| DesktopError::WaylandConnection(e.to_string()))?;
            guard.clear_ready();

            // Read and dispatch events
            if let Some(read_guard) = conn.prepare_read() {
                let _ = read_guard.read();
            }
            event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| DesktopError::WaylandConnection(format!("dispatch: {e}")))?;

            // Send pending events
            for event in state.pending_events.drain(..) {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}
