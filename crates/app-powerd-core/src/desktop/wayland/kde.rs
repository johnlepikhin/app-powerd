//! KDE Plasma focus tracking through a temporary KWin script.
//!
//! The backend owns a session D-Bus endpoint and loads the bundled `kwin.js`
//! into KWin at runtime. The script observes window activation, closure, and
//! fullscreen changes, then sends window metadata as JSON to that endpoint.
//! This module maps KWin's UUID window identifiers to the numeric IDs used by
//! [`FocusEvent`] and enriches events with process metadata from `/proc`.
//!
//! The script is unloaded and its temporary runtime file is removed when the
//! backend stops, including cancellation through [`ScriptGuard`].

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::desktop::window::WindowInfo;
use crate::desktop::{FocusBackend, FocusEvent};
use crate::error::DesktopError;

const KWIN_BUS: &str = "org.kde.KWin";
const KWIN_SCRIPTING_PATH: &str = "/Scripting";
const KWIN_SCRIPTING_INTERFACE: &str = "org.kde.kwin.Scripting";
const BRIDGE_BUS: &str = "io.github.johnlepikhin.AppPowerd.KWin";
const BRIDGE_PATH: &str = "/io/github/johnlepikhin/AppPowerd/KWin";
const PLUGIN_NAME: &str = "app-powerd";
const KWIN_SCRIPT: &str = include_str!("kwin.js");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum EventKind {
    Focused,
    Closed,
}

#[derive(Debug, Deserialize)]
struct KdeEvent {
    kind: EventKind,
    window: Option<KdeWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KdeWindow {
    id: String,
    pid: i32,
    title: String,
    app_id: String,
    resource_class: String,
    fullscreen: bool,
}

struct EventMapper {
    window_ids: HashMap<String, u64>,
    next_window_id: u64,
    pid_cache: HashMap<u32, crate::system::process::CachedProcessInfo>,
}

impl EventMapper {
    fn new() -> Self {
        Self {
            window_ids: HashMap::new(),
            next_window_id: 1,
            pid_cache: HashMap::new(),
        }
    }

    fn map(&mut self, json: &str) -> Result<Option<FocusEvent>, serde_json::Error> {
        let event: KdeEvent = serde_json::from_str(json)?;
        let Some(window) = event.window else {
            return Ok(None);
        };

        match event.kind {
            EventKind::Focused => {
                let window_id = self.window_ids.get(&window.id).copied().unwrap_or_else(|| {
                    let id = self.next_window_id;
                    self.next_window_id += 1;
                    self.window_ids.insert(window.id.clone(), id);
                    id
                });
                Ok(Some(FocusEvent::FocusChanged(
                    window.to_info(window_id, &mut self.pid_cache),
                )))
            }
            EventKind::Closed => Ok(self
                .window_ids
                .remove(&window.id)
                .map(|window_id| FocusEvent::WindowClosed { window_id })),
        }
    }
}

impl KdeWindow {
    fn to_info(
        &self,
        window_id: u64,
        pid_cache: &mut HashMap<u32, crate::system::process::CachedProcessInfo>,
    ) -> WindowInfo {
        let mut info = WindowInfo::new(window_id);
        info.title = Some(self.title.clone());
        info.app_id = Some(if self.app_id.is_empty() {
            self.resource_class.clone()
        } else {
            self.app_id.clone()
        });
        info.wm_class = Some(self.resource_class.clone());
        info.is_fullscreen = self.fullscreen;

        if self.pid > 0 {
            let pid = self.pid as u32;
            info.pid = Some(pid);
            let cached = pid_cache.entry(pid).or_insert_with(|| {
                let exe = crate::system::process::exe_name(pid).unwrap_or_default();
                let cmdline = crate::system::process::cmdline(pid).ok();
                crate::system::process::CachedProcessInfo { exe, cmdline }
            });
            info.executable = Some(cached.exe.clone());
            info.cmdline = cached.cmdline.clone();
        }

        info
    }
}

struct KdeEventSink {
    event_tx: mpsc::Sender<String>,
}

#[zbus::interface(name = "io.github.johnlepikhin.AppPowerd.KWin")]
impl KdeEventSink {
    async fn event(&self, json: String) {
        let _ = self.event_tx.send(json).await;
    }
}

pub struct KdeKWinBackend;

impl KdeKWinBackend {
    pub fn new() -> Result<Self, DesktopError> {
        let connection =
            zbus::blocking::Connection::session().map_err(|e| kde_error("D-Bus session", e))?;
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            KWIN_BUS,
            KWIN_SCRIPTING_PATH,
            KWIN_SCRIPTING_INTERFACE,
        )
        .map_err(|e| kde_error("KWin scripting proxy", e))?;
        let _: bool = proxy
            .call("isScriptLoaded", &PLUGIN_NAME)
            .map_err(|e| kde_error("KWin scripting not available", e))?;

        Ok(Self)
    }
}

#[async_trait::async_trait]
impl FocusBackend for KdeKWinBackend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        let (event_tx, mut event_rx) = mpsc::channel::<String>(64);
        let connection = zbus::connection::Builder::session()
            .map_err(|e| kde_error("D-Bus session", e))?
            .name(BRIDGE_BUS)
            .map_err(|e| kde_error("D-Bus bridge name", e))?
            .serve_at(BRIDGE_PATH, KdeEventSink { event_tx })
            .map_err(|e| kde_error("D-Bus bridge object", e))?
            .build()
            .await
            .map_err(|e| kde_error("D-Bus bridge connection", e))?;

        let mut script = ScriptGuard::create()?;
        let script_path_str = script.path.to_string_lossy();
        let proxy = zbus::Proxy::new(
            &connection,
            KWIN_BUS,
            KWIN_SCRIPTING_PATH,
            KWIN_SCRIPTING_INTERFACE,
        )
        .await
        .map_err(|e| kde_error("KWin scripting proxy", e))?;

        let already_loaded: bool = proxy
            .call("isScriptLoaded", &PLUGIN_NAME)
            .await
            .map_err(|e| kde_error("check KWin script", e))?;
        if already_loaded {
            let _: bool = proxy
                .call("unloadScript", &PLUGIN_NAME)
                .await
                .map_err(|e| kde_error("unload stale KWin script", e))?;
        }

        let script_id: i32 = proxy
            .call("loadScript", &(script_path_str.as_ref(), PLUGIN_NAME))
            .await
            .map_err(|e| kde_error("load KWin script", e))?;
        if script_id < 0 {
            return Err(DesktopError::WaylandConnection(
                "KDE load KWin script: KWin rejected the script".into(),
            ));
        }
        script.loaded = true;
        proxy
            .call::<_, _, ()>("start", &())
            .await
            .map_err(|e| kde_error("start KWin script", e))?;

        debug!(path = %script.path.display(), "KWin focus bridge started");
        let mut mapper = EventMapper::new();
        loop {
            tokio::select! {
                _ = tx.closed() => break,
                json = event_rx.recv() => {
                    let Some(json) = json else {
                        break;
                    };
                    match mapper.map(&json) {
                        Ok(Some(event)) => {
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => warn!(%error, "invalid event from KWin focus bridge"),
                    }
                }
            }
        }

        match proxy.call::<_, _, bool>("unloadScript", &PLUGIN_NAME).await {
            Ok(_) => script.loaded = false,
            Err(error) => warn!(%error, "failed to unload KWin focus bridge"),
        }

        Ok(())
    }
}

struct ScriptGuard {
    path: PathBuf,
    loaded: bool,
}

impl ScriptGuard {
    fn create() -> Result<Self, DesktopError> {
        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", nix::unistd::getuid().as_raw()));
        let path =
            PathBuf::from(runtime_dir).join(format!("app-powerd-kwin-{}.js", std::process::id()));
        std::fs::write(&path, KWIN_SCRIPT).map_err(|e| kde_error("write KWin script", e))?;
        Ok(Self {
            path,
            loaded: false,
        })
    }
}

impl Drop for ScriptGuard {
    fn drop(&mut self) {
        if self.loaded {
            let unload_result = zbus::blocking::Connection::session().and_then(|connection| {
                zbus::blocking::Proxy::new(
                    &connection,
                    KWIN_BUS,
                    KWIN_SCRIPTING_PATH,
                    KWIN_SCRIPTING_INTERFACE,
                )
                .and_then(|proxy| proxy.call::<_, _, bool>("unloadScript", &PLUGIN_NAME))
            });
            if let Err(error) = unload_result {
                warn!(%error, "failed to unload KWin focus bridge during cleanup");
            }
        }

        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(%error, path = %self.path.display(), "failed to remove KWin focus bridge script");
            }
        }
    }
}

fn kde_error(context: &str, error: impl std::fmt::Display) -> DesktopError {
    DesktopError::WaylandConnection(format!("KDE {context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOCUSED: &str = r#"{
        "kind":"focused",
        "window":{
            "id":"{cb68a747-6828-4df7-9426-512dddc23b14}",
            "pid":0,
            "title":"Konsole",
            "appId":"org.kde.konsole",
            "resourceClass":"org.kde.konsole",
            "fullscreen":false
        }
    }"#;

    #[test]
    fn focused_and_closed_events_share_window_id() {
        let mut mapper = EventMapper::new();

        let Some(FocusEvent::FocusChanged(window)) = mapper.map(FOCUSED).unwrap() else {
            panic!("expected focus event");
        };
        assert_eq!(window.window_id, 1);
        assert_eq!(window.app_id.as_deref(), Some("org.kde.konsole"));
        assert_eq!(window.wm_class.as_deref(), Some("org.kde.konsole"));

        let closed = FOCUSED.replace("\"focused\"", "\"closed\"");
        assert!(matches!(
            mapper.map(&closed).unwrap(),
            Some(FocusEvent::WindowClosed { window_id: 1 })
        ));
    }

    #[test]
    fn ignores_closed_window_that_was_never_focused() {
        let mut mapper = EventMapper::new();
        let closed = FOCUSED.replace("\"focused\"", "\"closed\"");

        assert!(mapper.map(&closed).unwrap().is_none());
    }

    #[test]
    fn accepts_window_without_pid() {
        let mut mapper = EventMapper::new();
        let focused = FOCUSED.replace("\"pid\":0", "\"pid\":-1");

        let Some(FocusEvent::FocusChanged(window)) = mapper.map(&focused).unwrap() else {
            panic!("expected focus event");
        };
        assert_eq!(window.pid, None);
    }
}
