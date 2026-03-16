use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

/// Timeout when waiting for the WindowsChanged D-Bus signal before falling back to a poll query.
const SIGNAL_POLL_TIMEOUT: Duration = Duration::from_secs(2);
/// Polling interval used when signal subscription is unavailable.
const POLLING_INTERVAL: Duration = Duration::from_millis(250);
use tracing::{debug, warn};
use zbus::blocking::Connection;
use zbus::zvariant::OwnedValue;

use crate::desktop::window::WindowInfo;
use crate::desktop::{FocusBackend, FocusEvent};
use crate::error::DesktopError;

const INTROSPECT_BUS: &str = "org.gnome.Shell.Introspect";
const INTROSPECT_PATH: &str = "/org/gnome/Shell/Introspect";

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
pub struct GnomeIntrospectBackend;

impl GnomeIntrospectBackend {
    pub fn new() -> Result<Self, DesktopError> {
        // Verify GNOME Shell Introspect is available
        let conn = Connection::session()
            .map_err(|e| DesktopError::WaylandConnection(format!("D-Bus session: {e}")))?;

        conn.call_method(
            Some(INTROSPECT_BUS),
            INTROSPECT_PATH,
            Some(INTROSPECT_BUS),
            "GetWindows",
            &(),
        )
        .map_err(|e| {
            DesktopError::WaylandConnection(format!("GNOME Shell Introspect not available: {e}"))
        })?;

        Ok(Self)
    }
}

#[async_trait::async_trait]
impl FocusBackend for GnomeIntrospectBackend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        // Use blocking D-Bus in a spawn_blocking thread
        let (event_tx, mut event_rx) = mpsc::channel::<FocusEvent>(64);
        let token = tokio_util::sync::CancellationToken::new();
        let thread_token = token.clone();

        let span = tracing::info_span!("gnome_introspect");
        tokio::task::spawn_blocking(move || {
            let _guard = span.entered();
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
            let mut pid_cache: HashMap<u32, crate::system::process::CachedProcessInfo> =
                HashMap::new();

            // Initial query
            if let Some(windows) = query_windows(&conn) {
                for (id, state) in &windows {
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
                if thread_token.is_cancelled() {
                    debug!("GNOME backend: cancellation requested, exiting");
                    return;
                }

                if let Some(ref rx) = signal_rx {
                    // Wait for signal with fallback timeout
                    let _ = rx.recv_timeout(SIGNAL_POLL_TIMEOUT);
                } else {
                    std::thread::sleep(POLLING_INTERVAL);
                }

                let Some(windows) = query_windows(&conn) else {
                    continue;
                };

                // Detect focus changes
                for (id, state) in &windows {
                    if state.is_focused
                        && (last_focused != Some(*id) || state.is_fullscreen != last_fullscreen)
                    {
                        let info = window_state_to_info(*id, state, &mut pid_cache);
                        debug!(window_id = id, app_id = %state.app_id, "GNOME focus changed");
                        last_focused = Some(*id);
                        last_fullscreen = state.is_fullscreen;
                        if event_tx
                            .blocking_send(FocusEvent::FocusChanged(info))
                            .is_err()
                        {
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

        token.cancel();
        Ok(())
    }
}

fn window_state_to_info(
    id: u64,
    state: &WindowState,
    pid_cache: &mut HashMap<u32, crate::system::process::CachedProcessInfo>,
) -> WindowInfo {
    let mut info = WindowInfo::new(id);
    info.title = Some(state.title.clone());
    info.app_id = Some(state.app_id.clone());
    info.wm_class = Some(state.app_id.clone());
    info.is_fullscreen = state.is_fullscreen;
    if state.pid > 0 {
        info.pid = Some(state.pid);
        let cached = pid_cache.entry(state.pid).or_insert_with(|| {
            let exe = crate::system::process::exe_name(state.pid).unwrap_or_default();
            let cmdline = crate::system::process::cmdline(state.pid).ok();
            crate::system::process::CachedProcessInfo { exe, cmdline }
        });
        info.executable = Some(cached.exe.clone());
        info.cmdline = cached.cmdline.clone();
    }
    info
}

/// Extract a typed property from D-Bus window properties.
fn get_prop<T: TryFrom<OwnedValue>>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    props.get(key).and_then(|v| T::try_from(v.clone()).ok())
}

fn query_windows(conn: &Connection) -> Option<HashMap<u64, WindowState>> {
    let reply = conn
        .call_method(
            Some(INTROSPECT_BUS),
            INTROSPECT_PATH,
            Some(INTROSPECT_BUS),
            "GetWindows",
            &(),
        )
        .map_err(|e| {
            debug!(error = %e, "GetWindows D-Bus call failed");
            e
        })
        .ok()?;

    let body: HashMap<u64, HashMap<String, OwnedValue>> = reply
        .body()
        .deserialize()
        .map_err(|e| {
            debug!(error = %e, "failed to deserialize GetWindows response");
            e
        })
        .ok()?;

    let mut result = HashMap::new();
    for (id, props) in body {
        let title: String = get_prop(&props, "title").unwrap_or_default();
        let app_id: String = get_prop(&props, "app-id")
            .or_else(|| get_prop(&props, "wm-class"))
            .unwrap_or_default();
        let pid: u32 = get_prop(&props, "pid").unwrap_or(0);
        let is_focused: bool = get_prop(&props, "focus").unwrap_or(false);
        let is_fullscreen: bool = get_prop(&props, "fullscreen").unwrap_or(false);

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
    use zbus::message::Type;
    use zbus::MatchRule;

    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .interface(INTROSPECT_BUS)
        .ok()?
        .member("WindowsChanged")
        .ok()?
        .build();

    let iter = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(64)).ok()?;

    let (tx, rx) = std::sync::mpsc::sync_channel(1);

    std::thread::Builder::new()
        .name("gnome-signal-watcher".into())
        .spawn(move || {
            for _msg in iter {
                match tx.try_send(()) {
                    Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => {}
                    Err(std::sync::mpsc::TrySendError::Disconnected(())) => break,
                }
            }
        })
        .ok()?;

    Some(rx)
}
