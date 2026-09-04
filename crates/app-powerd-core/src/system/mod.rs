pub(crate) mod apply;
pub(crate) mod cgroup;
pub(crate) mod freeze;
/// Power source detection.
pub mod power;
/// Process identity and `/proc` inspection.
pub mod process;
pub(crate) mod protection;
pub(crate) mod systemd_dbus;
pub(crate) mod throttle;

pub use process::ProcessHandle;

use std::path::PathBuf;

/// Per-user runtime directory for sockets, locks and the freeze journal.
///
/// The daemon and a standalone CLI invocation must agree on this path or
/// emergency recovery silently finds nothing, so the fallback lives here rather
/// than being duplicated at each call site.
pub fn runtime_dir() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", nix::unistd::getuid().as_raw()));
    PathBuf::from(dir)
}

/// Sanitize a string to be a valid systemd unit name component.
pub(crate) fn sanitize_unit_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Get the base cgroup path for the current user.
///
/// Reads `/proc/self/cgroup` to detect the actual cgroup v2 path (works on any system),
/// falls back to systemd convention if unavailable.
pub(crate) fn cgroup_base_path() -> PathBuf {
    if let Ok(content) = std::fs::read_to_string("/proc/self/cgroup") {
        for line in content.lines() {
            if let Some(path) = line.strip_prefix("0::") {
                let cgroup_path =
                    PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/'));
                if cgroup_path.exists() {
                    return cgroup_path;
                }
            }
        }
    }

    // Fallback: systemd convention
    let uid = nix::unistd::getuid().as_raw();
    PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_simple_name() {
        assert_eq!(sanitize_unit_name("firefox"), "firefox");
    }

    #[test]
    fn sanitize_dots_to_dashes() {
        assert_eq!(
            sanitize_unit_name("org.telegram.desktop"),
            "org-telegram-desktop"
        );
    }

    #[test]
    fn sanitize_special_chars() {
        let result = sanitize_unit_name("app/with spaces");
        assert!(!result.contains('/'));
        assert!(!result.contains(' '));
        assert_eq!(result, "app-with-spaces");
    }
}
