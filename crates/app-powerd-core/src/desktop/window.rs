/// Information about a desktop window.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// Window ID (X11 window ID or Wayland toplevel handle).
    pub window_id: u64,
    /// Process ID owning the window.
    pub pid: Option<u32>,
    /// Window title.
    pub title: Option<String>,
    /// WM_CLASS (X11) or app_id (Wayland).
    pub wm_class: Option<String>,
    /// Wayland app_id.
    pub app_id: Option<String>,
    /// Executable path (resolved from PID).
    pub executable: Option<String>,
    /// Command line (resolved from PID).
    pub cmdline: Option<String>,
    /// Whether the window is fullscreen.
    pub is_fullscreen: bool,
}

impl WindowInfo {
    /// Create a new WindowInfo with just a window_id.
    pub fn new(window_id: u64) -> Self {
        Self {
            window_id,
            pid: None,
            title: None,
            wm_class: None,
            app_id: None,
            executable: None,
            cmdline: None,
            is_fullscreen: false,
        }
    }
}
