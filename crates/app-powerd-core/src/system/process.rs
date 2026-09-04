use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::SystemError;

/// Stable identity of a running process: its PID together with the boot-relative
/// start time from `/proc/<pid>/stat`.
///
/// A bare PID is not an identity — the kernel recycles PIDs, so a PID captured
/// at freeze time may name an unrelated process by the time we try to thaw it.
/// Pairing it with `starttime` makes the identity stable for the lifetime of the
/// process and detectable as stale afterwards, which is what lets the daemon
/// reap dead entries and safely restore state from a journal written by a
/// previous run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProcessHandle {
    pub pid: u32,
    pub starttime: u64,
}

impl ProcessHandle {
    /// Capture the identity of a currently running process.
    ///
    /// Returns `None` if the process does not exist or `/proc/<pid>/stat` cannot
    /// be parsed.
    pub fn open(pid: u32) -> Option<Self> {
        if pid == 0 {
            return None;
        }
        read_starttime(pid).map(|starttime| Self { pid, starttime })
    }

    /// Whether the process this handle names is still the same live process.
    ///
    /// A recycled PID yields a different `starttime` and is reported as dead,
    /// which is the point: acting on it would target a stranger.
    pub fn is_alive(&self) -> bool {
        read_starttime(self.pid) == Some(self.starttime)
    }

    /// Whether the process is owned by the current user.
    ///
    /// Deliberately separate from [`is_alive`](Self::is_alive): identity says
    /// *which* process this is, ownership says whether we may signal it. Both
    /// are required before sending a signal on behalf of an IPC client or when
    /// restoring from the journal.
    pub fn is_owned(&self) -> bool {
        is_owned_pid(self.pid)
    }
}

/// Read field 22 (`starttime`) of `/proc/<pid>/stat`.
fn read_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_starttime(&stat)
}

/// Extract field 22 (`starttime`) from the contents of a `/proc/<pid>/stat`
/// record.
///
/// The `comm` field (2) is wrapped in parentheses and may itself contain spaces
/// and parentheses, so the record is split at the *last* `)` rather than
/// tokenized from the start. After that separator the remaining whitespace-
/// separated fields begin at field 3, putting `starttime` at index 19.
///
/// Kept separate from the file read so the parsing — the part everything else
/// depends on for process identity — can be tested against hostile input
/// without a matching process having to exist.
fn parse_starttime(stat: &str) -> Option<u64> {
    let tail = &stat[stat.rfind(')')? + 1..];
    tail.split_whitespace().nth(19)?.parse().ok()
}

/// Check if the given PID belongs to the current user.
pub fn is_owned_pid(pid: u32) -> bool {
    let Ok(metadata) = fs::metadata(format!("/proc/{pid}")) else {
        return false;
    };
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == nix::unistd::getuid().as_raw()
}

/// Read /proc/PID/exe symlink to get executable path.
pub(crate) fn exe_name(pid: u32) -> Result<String, SystemError> {
    let link = fs::read_link(format!("/proc/{pid}/exe")).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SystemError::ProcessNotFound { pid }
        } else {
            SystemError::ProcessReadError {
                pid,
                message: format!("failed to read exe: {e}"),
            }
        }
    })?;
    // Extract just the filename
    Ok(link
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| link.to_string_lossy().to_string()))
}

/// Read `/proc/PID/comm` — the kernel's short process name.
///
/// Used as a fallback identity when `/proc/<pid>/exe` is unreadable, which is
/// common for processes the daemon did not launch. Returns `None` rather than an
/// error because callers treat "no name" as its own signal.
pub fn comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read /proc/PID/cmdline.
pub(crate) fn cmdline(pid: u32) -> Result<String, SystemError> {
    let data = fs::read(format!("/proc/{pid}/cmdline")).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            SystemError::ProcessNotFound { pid }
        } else {
            SystemError::ProcessReadError {
                pid,
                message: format!("failed to read cmdline: {e}"),
            }
        }
    })?;
    // cmdline is null-separated
    Ok(data
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string())
}

/// Collect all descendant PIDs of the given PID (recursive).
///
/// Walks `/proc/<pid>/task/<tid>/children` to find all transitive children.
/// Returns an empty vec if the process has no children or on any error.
pub(crate) fn descendant_pids(pid: u32) -> Vec<u32> {
    let mut result = Vec::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        let task_dir = format!("/proc/{current}/task");
        let Ok(entries) = fs::read_dir(&task_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let children_path = entry.path().join("children");
            let Ok(content) = fs::read_to_string(&children_path) else {
                continue;
            };
            for token in content.split_whitespace() {
                if let Ok(child_pid) = token.parse::<u32>() {
                    result.push(child_pid);
                    stack.push(child_pid);
                }
            }
        }
    }
    result
}

/// Cached process info to avoid repeated /proc reads.
pub(crate) struct CachedProcessInfo {
    /// Executable basename.
    pub exe: String,
    /// Command line, if available.
    pub cmdline: Option<String>,
}

/// Build a map of executable name → desktop file ID from .desktop files.
/// Scans /usr/share/applications/ and $XDG_DATA_HOME/applications/.
pub(crate) fn build_desktop_index() -> std::collections::HashMap<String, String> {
    use tracing::debug;

    let mut index = std::collections::HashMap::new();
    let mut dirs: Vec<PathBuf> = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string())
        .split(':')
        .map(|d| PathBuf::from(d).join("applications"))
        .collect();

    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let desktop_id = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();

            if let Ok(content) = fs::read_to_string(&path) {
                if let Some(exec_name) = parse_exec_from_desktop(&content) {
                    debug!(desktop_id = %desktop_id, exec = %exec_name, "indexed desktop file");
                    index.insert(exec_name, desktop_id);
                }
            }
        }
    }

    index
}

/// Extract the executable name from the Exec= line of a .desktop file.
fn parse_exec_from_desktop(content: &str) -> Option<String> {
    let mut in_desktop_entry = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_desktop_entry = trimmed == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry {
            continue;
        }
        if let Some(exec_val) = trimmed.strip_prefix("Exec=") {
            // Take the first token, strip any leading path
            let cmd = exec_val.split_whitespace().next()?;
            // Remove env vars like `env VAR=val cmd`
            let cmd = if cmd == "env" {
                // Skip env and VAR=val pairs
                exec_val
                    .split_whitespace()
                    .skip(1)
                    .find(|t| !t.contains('='))?
            } else {
                cmd
            };
            // Extract just the filename from path
            let name = std::path::Path::new(cmd).file_name()?.to_str()?.to_string();
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parser must survive a `comm` containing both spaces and parentheses,
    /// which is legal and appears in the wild (e.g. `(sd-pam)`, JVM threads).
    #[test]
    fn starttime_index_after_last_paren() {
        // Fields 1..=22 with a hostile comm; starttime (field 22) is 987654.
        let stat = "42 (weird ) name) S 1 42 42 0 -1 4194304 100 0 0 0 5 6 0 0 20 0 1 0 987654 \
                    12345678 900 18446744073709551615";
        assert_eq!(parse_starttime(stat), Some(987654));
    }

    /// A plain name must land on the same field — guards against an off-by-one
    /// that a parenthesised-comm case alone would not reveal.
    #[test]
    fn starttime_with_ordinary_comm() {
        let stat = "1 (systemd) S 0 1 1 0 -1 4194560 200 0 0 0 10 20 0 0 20 0 1 0 4242 \
                    170000000 3000 18446744073709551615";
        assert_eq!(parse_starttime(stat), Some(4242));
    }

    #[test]
    fn starttime_rejects_malformed_records() {
        assert_eq!(parse_starttime(""), None);
        assert_eq!(parse_starttime("42 no-parens S 1"), None);
        // Truncated: fewer than 22 fields.
        assert_eq!(parse_starttime("42 (sh) S 1 42 42 0 -1"), None);
    }

    #[test]
    fn process_handle_open_self_is_alive() {
        let handle = ProcessHandle::open(std::process::id()).expect("own process is readable");
        assert!(handle.is_alive());
        assert!(handle.is_owned());
    }

    #[test]
    fn process_handle_rejects_pid_zero() {
        assert!(ProcessHandle::open(0).is_none());
    }

    /// A recycled PID must read as dead: same pid, different starttime.
    #[test]
    fn process_handle_detects_starttime_mismatch() {
        let real = ProcessHandle::open(std::process::id()).unwrap();
        let impostor = ProcessHandle {
            pid: real.pid,
            starttime: real.starttime.wrapping_add(1),
        };
        assert!(!impostor.is_alive());
    }

    #[test]
    fn process_handle_open_reaped_child_is_none() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn /bin/true");
        let pid = child.id();
        child.wait().expect("reap child");
        // After reaping, /proc/<pid> is gone, so no identity can be captured.
        assert!(
            ProcessHandle::open(pid).is_none() || !ProcessHandle::open(pid).unwrap().is_alive()
        );
    }

    #[test]
    fn comm_of_self_is_non_empty() {
        assert!(comm(std::process::id()).is_some());
    }

    #[test]
    fn read_self_exe_name() {
        let pid = std::process::id();
        let name = exe_name(pid).unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn read_self_cmdline() {
        let pid = std::process::id();
        let cmd = cmdline(pid).unwrap();
        assert!(!cmd.is_empty());
    }

    #[test]
    fn parse_exec_simple_path() {
        assert_eq!(
            super::parse_exec_from_desktop("[Desktop Entry]\nExec=/usr/bin/firefox %u\n"),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn parse_exec_with_env_prefix() {
        assert_eq!(
            super::parse_exec_from_desktop(
                "[Desktop Entry]\nExec=env GDK_BACKEND=wayland telegram-desktop\n"
            ),
            Some("telegram-desktop".to_string())
        );
    }

    #[test]
    fn parse_exec_with_multiple_env_vars() {
        assert_eq!(
            super::parse_exec_from_desktop(
                "[Desktop Entry]\nExec=env VAR1=a VAR2=b /opt/app/myapp --flag\n"
            ),
            Some("myapp".to_string())
        );
    }

    #[test]
    fn parse_exec_bare_command() {
        assert_eq!(
            super::parse_exec_from_desktop("[Desktop Entry]\nExec=code --unity-launch %F\n"),
            Some("code".to_string())
        );
    }

    #[test]
    fn parse_exec_no_exec_line() {
        assert_eq!(
            super::parse_exec_from_desktop("[Desktop Entry]\nName=Test\n"),
            None
        );
    }
}
