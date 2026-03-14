use std::fs;
use std::path::PathBuf;

use crate::error::SystemError;

/// Read /proc/PID/exe symlink to get executable path.
pub fn exe(pid: u32) -> Result<String, SystemError> {
    let link = fs::read_link(format!("/proc/{pid}/exe"))
        .map_err(|_| SystemError::ProcessNotFound { pid })?;
    // Extract just the filename
    Ok(link
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| link.to_string_lossy().to_string()))
}

/// Read /proc/PID/cmdline.
pub fn cmdline(pid: u32) -> Result<String, SystemError> {
    let data = fs::read(format!("/proc/{pid}/cmdline"))
        .map_err(|_| SystemError::ProcessNotFound { pid })?;
    // cmdline is null-separated
    Ok(data
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string())
}

/// Read /proc/PID/cgroup to get current cgroup path.
pub fn cgroup_path(pid: u32) -> Result<String, SystemError> {
    let content = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|_| SystemError::ProcessNotFound { pid })?;
    // cgroup v2 format: "0::/path"
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Ok(path.to_string());
        }
    }
    Err(SystemError::CgroupError {
        message: format!("no cgroup v2 entry for pid {pid}"),
    })
}

/// Collect all descendant PIDs of the given PID (recursive).
///
/// Walks `/proc/<pid>/task/<tid>/children` to find all transitive children.
/// Returns an empty vec if the process has no children or on any error.
pub fn descendant_pids(pid: u32) -> Vec<u32> {
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

/// Build a map of executable name → desktop file ID from .desktop files.
/// Scans /usr/share/applications/ and $XDG_DATA_HOME/applications/.
pub fn build_desktop_index() -> std::collections::HashMap<String, String> {
    use tracing::debug;

    let mut index = std::collections::HashMap::new();
    let mut dirs = vec![PathBuf::from("/usr/share/applications")];

    if let Ok(data_home) = std::env::var("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home).join("applications"));
    } else if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
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
    for line in content.lines() {
        let trimmed = line.trim();
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
            let name = std::path::Path::new(cmd)
                .file_name()?
                .to_str()?
                .to_string();
            return Some(name);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_self_exe() {
        let pid = std::process::id();
        let name = exe(pid).unwrap();
        assert!(!name.is_empty());
    }

    #[test]
    fn read_self_cmdline() {
        let pid = std::process::id();
        let cmd = cmdline(pid).unwrap();
        assert!(!cmd.is_empty());
    }

    #[test]
    fn read_self_cgroup() {
        let pid = std::process::id();
        // May fail in some environments but shouldn't panic
        let _ = cgroup_path(pid);
    }
}
