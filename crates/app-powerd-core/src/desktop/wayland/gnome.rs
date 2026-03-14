use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tracing::{debug, warn};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

use crate::desktop::window::WindowInfo;
use crate::desktop::{FocusBackend, FocusEvent};
use crate::error::DesktopError;

/// Window state from GNOME Shell Introspect.
#[derive(Debug, Clone)]
struct WindowState {
    title: String,
    app_id: String,
    pid: u32,
    is_focused: bool,
    is_fullscreen: bool,
}

/// GNOME Shell Introspect backend via D-Bus.
pub struct GnomeIntrospectBackend {
    fullscreen_state: Arc<Mutex<HashMap<u64, bool>>>,
}

impl GnomeIntrospectBackend {
    pub fn new() -> Result<Self, DesktopError> {
        // Verify GNOME Shell Introspect is available
        let conn = Connection::session()
            .map_err(|e| DesktopError::WaylandConnection(format!("D-Bus session: {e}")))?;

        conn.call_method(
            Some("org.gnome.Shell.Introspect"),
            "/org/gnome/Shell/Introspect",
            Some("org.gnome.Shell.Introspect"),
            "GetWindows",
            &(),
        )
        .map_err(|e| {
            DesktopError::WaylandConnection(format!("GNOME Shell Introspect not available: {e}"))
        })?;

        Ok(Self {
            fullscreen_state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn is_fullscreen(&self, window_id: u64) -> bool {
        self.fullscreen_state
            .lock()
            .map(|s| s.get(&window_id).copied().unwrap_or(false))
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl FocusBackend for GnomeIntrospectBackend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        let fullscreen_state = self.fullscreen_state.clone();

        // Use blocking D-Bus in a spawn_blocking thread
        let (event_tx, mut event_rx) = mpsc::channel::<FocusEvent>(64);

        tokio::task::spawn_blocking(move || {
            let conn = match Connection::session() {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "failed to connect to D-Bus session");
                    return;
                }
            };

            let mut last_focused: Option<u64> = None;
            let mut last_fullscreen: bool = false;
            let mut known_windows: HashMap<u64, WindowState> = HashMap::new();
            let mut pid_cache: HashMap<u32, (String, Option<String>)> = HashMap::new();

            // Initial query
            if let Some(windows) = query_windows(&conn) {
                for (id, state) in &windows {
                    fullscreen_state
                        .lock()
                        .map(|mut s| s.insert(*id, state.is_fullscreen))
                        .ok();

                    if state.is_focused {
                        let info = window_state_to_info(*id, state, &mut pid_cache);
                        last_focused = Some(*id);
                        last_fullscreen = state.is_fullscreen;
                        let _ = event_tx.blocking_send(FocusEvent::FocusChanged(info));
                    }
                }
                known_windows = windows;
            }

            // Try to subscribe to WindowsChanged signal for event-driven updates.
            // Signal watcher runs in a separate thread and notifies via channel.
            // Falls back to polling if signal subscription fails.
            let signal_rx = subscribe_windows_changed(&conn);
            if signal_rx.is_some() {
                debug!("GNOME: subscribed to WindowsChanged signal, using 2s fallback poll");
            } else {
                debug!("GNOME: falling back to 250ms polling (signal subscription failed)");
            }

            loop {
                if let Some(ref rx) = signal_rx {
                    // Wait for signal with 2s timeout as fallback
                    let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }

                let Some(windows) = query_windows(&conn) else {
                    continue;
                };

                // Detect focus changes
                for (id, state) in &windows {
                    fullscreen_state
                        .lock()
                        .map(|mut s| s.insert(*id, state.is_fullscreen))
                        .ok();

                    if state.is_focused && (last_focused != Some(*id) || state.is_fullscreen != last_fullscreen) {
                        let info = window_state_to_info(*id, state, &mut pid_cache);
                        debug!(window_id = id, app_id = %state.app_id, "GNOME focus changed");
                        last_focused = Some(*id);
                        last_fullscreen = state.is_fullscreen;
                        if event_tx.blocking_send(FocusEvent::FocusChanged(info)).is_err() {
                            return;
                        }
                    }
                }

                // Detect closed windows
                let closed: Vec<u64> = known_windows
                    .keys()
                    .filter(|id| !windows.contains_key(id))
                    .copied()
                    .collect();

                for id in closed {
                    debug!(window_id = id, "GNOME window closed");
                    fullscreen_state
                        .lock()
                        .map(|mut s| s.remove(&id))
                        .ok();
                    if event_tx
                        .blocking_send(FocusEvent::WindowClosed { window_id: id })
                        .is_err()
                    {
                        return;
                    }
                }

                known_windows = windows;
            }
        });

        // Forward events from blocking thread to main channel
        while let Some(event) = event_rx.recv().await {
            if tx.send(event).await.is_err() {
                break;
            }
        }

        Ok(())
    }

    fn is_fullscreen(&self, window_id: u64) -> bool {
        self.is_fullscreen(window_id)
    }
}

fn window_state_to_info(
    id: u64,
    state: &WindowState,
    pid_cache: &mut HashMap<u32, (String, Option<String>)>,
) -> WindowInfo {
    let mut info = WindowInfo::new(id);
    info.title = Some(state.title.clone());
    info.app_id = Some(state.app_id.clone());
    info.wm_class = Some(state.app_id.clone());
    info.is_fullscreen = state.is_fullscreen;
    if state.pid > 0 {
        info.pid = Some(state.pid);
        let (exe, cmdline) = pid_cache
            .entry(state.pid)
            .or_insert_with(|| {
                let exe = crate::system::process::exe(state.pid).unwrap_or_default();
                let cmdline = crate::system::process::cmdline(state.pid).ok();
                (exe, cmdline)
            });
        info.executable = Some(exe.clone());
        info.cmdline = cmdline.clone();
    }
    info
}

fn query_windows(conn: &Connection) -> Option<HashMap<u64, WindowState>> {
    let reply = conn
        .call_method(
            Some("org.gnome.Shell.Introspect"),
            "/org/gnome/Shell/Introspect",
            Some("org.gnome.Shell.Introspect"),
            "GetWindows",
            &(),
        )
        .ok()?;

    let body: HashMap<u64, HashMap<String, OwnedValue>> = reply.body().deserialize().ok()?;

    let mut result = HashMap::new();
    for (id, props) in body {
        let title = props
            .get("title")
            .and_then(|v| <String>::try_from(v.clone()).ok())
            .unwrap_or_default();

        let app_id = props
            .get("app-id")
            .or_else(|| props.get("wm-class"))
            .and_then(|v| <String>::try_from(v.clone()).ok())
            .unwrap_or_default();

        let pid = props
            .get("pid")
            .and_then(|v| <u32>::try_from(v.clone()).ok())
            .unwrap_or(0);

        let is_focused = props
            .get("focus")
            .and_then(|v| <bool>::try_from(v.clone()).ok())
            .unwrap_or(false);

        let is_fullscreen = props
            .get("fullscreen")
            .and_then(|v| <bool>::try_from(v.clone()).ok())
            .unwrap_or(false);

        result.insert(
            id,
            WindowState {
                title,
                app_id,
                pid,
                is_focused,
                is_fullscreen,
            },
        );
    }

    Some(result)
}

/// Subscribe to WindowsChanged signal and return a channel receiver.
/// A dedicated thread listens for signals and sends notifications.
/// Returns None if subscription fails.
fn subscribe_windows_changed(conn: &Connection) -> Option<std::sync::mpsc::Receiver<()>> {
    use zbus::MatchRule;
    use zbus::message::Type;

    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface("org.gnome.Shell.Introspect").ok()?
        .member("WindowsChanged").ok()?
        .build();

    let iter = zbus::blocking::MessageIterator::for_match_rule(
        rule, conn, Some(64),
    ).ok()?;

    let (tx, rx) = std::sync::mpsc::sync_channel(1);

    std::thread::Builder::new()
        .name("gnome-signal-watcher".into())
        .spawn(move || {
            for _msg in iter {
                if tx.try_send(()).is_err() {
                    // Receiver dropped or channel full — either way, nothing to do
                }
            }
        })
        .ok()?;

    Some(rx)
}
