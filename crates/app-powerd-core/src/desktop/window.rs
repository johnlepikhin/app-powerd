/// Virtual desktop / workspace placement of a window per EWMH `_NET_WM_DESKTOP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Desktop {
    /// Window is shown on all virtual desktops ("sticky"): the EWMH sentinel
    /// `0xFFFFFFFF`.
    All,
    /// Window claims to live on a specific desktop index.
    Index(u32),
}

impl Desktop {
    /// EWMH sentinel meaning "show on all desktops".
    const STICKY: u32 = u32::MAX;

    /// Parse a raw `_NET_WM_DESKTOP` CARDINAL value.
    pub fn from_raw(raw: u32) -> Self {
        if raw == Self::STICKY {
            Self::All
        } else {
            Self::Index(raw)
        }
    }

    /// Whether this desktop is visible when `current` is the active workspace.
    pub fn matches(self, current: u32) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Index(i) if i == current)
    }
}

/// Information about a desktop window.
///
/// Marked `#[non_exhaustive]` so adding new fields is not a breaking change for
/// external consumers — construct via [`WindowInfo::new`] and set fields.
#[derive(Debug, Clone)]
#[non_exhaustive]
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
    /// `_NET_WM_DESKTOP` placement. Populated by the X11 backend only;
    /// Wayland/GNOME backends leave it `None`. `None` also means the property
    /// is unset on the window.
    pub desktop: Option<Desktop>,
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
            desktop: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_from_raw_sticky() {
        assert_eq!(Desktop::from_raw(u32::MAX), Desktop::All);
    }

    #[test]
    fn desktop_from_raw_index() {
        assert_eq!(Desktop::from_raw(3), Desktop::Index(3));
    }

    #[test]
    fn desktop_matches_index() {
        assert!(Desktop::Index(5).matches(5));
        assert!(!Desktop::Index(5).matches(6));
    }

    #[test]
    fn desktop_matches_all() {
        assert!(Desktop::All.matches(0));
        assert!(Desktop::All.matches(u32::MAX - 1));
    }
}
