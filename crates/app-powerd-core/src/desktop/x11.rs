use std::os::fd::AsRawFd;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tracing::{debug, info};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use super::window::WindowInfo;
use super::{FocusBackend, FocusEvent};
use crate::error::DesktopError;
use crate::system::process;

pub struct X11Backend {
    conn: RustConnection,
    root: u32,
    active_window_atom: u32,
    net_wm_pid_atom: u32,
    wm_class_atom: u32,
    net_wm_name_atom: u32,
    utf8_string_atom: u32,
    net_wm_state_atom: u32,
    net_wm_state_fullscreen_atom: u32,
}

impl X11Backend {
    pub fn new() -> Result<Self, DesktopError> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| DesktopError::X11Connection(e.to_string()))?;

        let screen = conn.setup().roots.get(screen_num).ok_or_else(|| {
            DesktopError::X11Connection(format!("invalid screen number: {screen_num}"))
        })?;
        let root = screen.root;

        change_window_attributes(
            &conn,
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| DesktopError::X11Connection(e.to_string()))?;
        conn.flush()
            .map_err(|e| DesktopError::X11Connection(e.to_string()))?;

        let intern = |name: &[u8]| -> Result<u32, DesktopError> {
            intern_atom(&conn, false, name)
                .map_err(|e| DesktopError::X11Connection(e.to_string()))?
                .reply()
                .map(|r| r.atom)
                .map_err(|e| DesktopError::X11Connection(e.to_string()))
        };

        Ok(Self {
            active_window_atom: intern(b"_NET_ACTIVE_WINDOW")?,
            net_wm_pid_atom: intern(b"_NET_WM_PID")?,
            wm_class_atom: intern(b"WM_CLASS")?,
            net_wm_name_atom: intern(b"_NET_WM_NAME")?,
            utf8_string_atom: intern(b"UTF8_STRING")?,
            net_wm_state_atom: intern(b"_NET_WM_STATE")?,
            net_wm_state_fullscreen_atom: intern(b"_NET_WM_STATE_FULLSCREEN")?,
            conn,
            root,
        })
    }

    fn get_property_reply(
        &self,
        window: u32,
        property: u32,
        type_: impl Into<u32>,
        long_length: u32,
    ) -> Option<GetPropertyReply> {
        get_property(&self.conn, false, window, property, type_, 0, long_length)
            .ok()?
            .reply()
            .ok()
    }

    fn get_active_window(&self) -> Option<u32> {
        let reply =
            self.get_property_reply(self.root, self.active_window_atom, AtomEnum::WINDOW, 1)?;

        if reply.value_len == 0 {
            return None;
        }

        let win = u32::from_ne_bytes(reply.value[..4].try_into().ok()?);
        if win == 0 || win == self.root {
            return None;
        }
        Some(win)
    }

    fn get_window_info(&self, window_id: u32) -> WindowInfo {
        let mut info = WindowInfo::new(window_id as u64);

        // PID
        if let Some(reply) =
            self.get_property_reply(window_id, self.net_wm_pid_atom, AtomEnum::CARDINAL, 1)
        {
            if reply.value_len > 0 {
                let pid = u32::from_ne_bytes(reply.value[..4].try_into().unwrap_or([0; 4]));
                if pid > 0 {
                    info.pid = Some(pid);
                    info.executable = process::exe_name(pid).ok();
                    info.cmdline = process::cmdline(pid).ok();
                }
            }
        }

        // WM_CLASS
        if let Some(reply) =
            self.get_property_reply(window_id, self.wm_class_atom, AtomEnum::STRING, 256)
        {
            if !reply.value.is_empty() {
                let parts: Vec<&[u8]> = reply.value.split(|&b| b == 0).collect();
                if parts.len() >= 2 {
                    info.wm_class = Some(String::from_utf8_lossy(parts[1]).to_string());
                }
            }
        }

        // _NET_WM_NAME (title)
        if let Some(reply) = self.get_property_reply(
            window_id,
            self.net_wm_name_atom,
            self.utf8_string_atom,
            1024,
        ) {
            if !reply.value.is_empty() {
                info.title = Some(String::from_utf8_lossy(&reply.value).to_string());
            }
        }

        info.is_fullscreen = self.check_fullscreen(window_id);
        info
    }

    fn check_fullscreen(&self, window_id: u32) -> bool {
        let Some(reply) =
            self.get_property_reply(window_id, self.net_wm_state_atom, AtomEnum::ATOM, 32)
        else {
            return false;
        };

        let atoms = parse_u32_slice(&reply.value);
        atoms.contains(&self.net_wm_state_fullscreen_atom)
    }
}

fn parse_u32_slice(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[async_trait::async_trait]
impl FocusBackend for X11Backend {
    async fn run(self: Box<Self>, tx: mpsc::Sender<FocusEvent>) -> Result<(), DesktopError> {
        let fd = AsyncFd::new(self.conn.stream().as_raw_fd())
            .map_err(|e| DesktopError::X11Connection(e.to_string()))?;

        // Send initial active window
        if let Some(win) = self.get_active_window() {
            let info = self.get_window_info(win);
            info!(window_id = win, wm_class = ?info.wm_class, "initial active window");
            let _ = tx.send(FocusEvent::FocusChanged(info)).await;
        }

        loop {
            let mut guard = fd
                .readable()
                .await
                .map_err(|e| DesktopError::X11Connection(e.to_string()))?;
            guard.clear_ready();

            while let Some(event) = self
                .conn
                .poll_for_event()
                .map_err(|e| DesktopError::X11Connection(e.to_string()))?
            {
                match event {
                    Event::PropertyNotify(ev) if ev.atom == self.active_window_atom => {
                        if let Some(win) = self.get_active_window() {
                            let info = self.get_window_info(win);
                            debug!(window_id = win, wm_class = ?info.wm_class, title = ?info.title, "focus changed");

                            let _ = change_window_attributes(
                                &self.conn,
                                win,
                                &ChangeWindowAttributesAux::new()
                                    .event_mask(EventMask::STRUCTURE_NOTIFY),
                            );
                            let _ = self.conn.flush();

                            if tx.send(FocusEvent::FocusChanged(info)).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Event::DestroyNotify(ev) => {
                        debug!(window_id = ev.window, "window destroyed");
                        if tx
                            .send(FocusEvent::WindowClosed {
                                window_id: ev.window as u64,
                            })
                            .await
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
