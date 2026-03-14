pub mod process;
pub mod cgroup;
pub mod throttle;
pub mod freeze;
pub mod power;
pub mod systemd_dbus;

use std::path::PathBuf;

/// Sanitize a string to be a valid systemd unit name component.
pub(crate) fn sanitize_unit_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
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
    PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"))
}
