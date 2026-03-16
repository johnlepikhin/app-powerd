use std::fs;
use std::path::PathBuf;

use tracing::debug;

/// Check if any of the given PIDs have /dev/video* open.
pub async fn is_using_camera(pids: &[u32]) -> bool {
    let pids = pids.to_vec();
    tokio::task::spawn_blocking(move || check_camera_sync(&pids))
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "camera check task failed");
            false
        })
}

fn check_camera_sync(pids: &[u32]) -> bool {
    for &pid in pids {
        if has_video_fd(pid) {
            debug!(pid, "camera in use");
            return true;
        }
    }
    false
}

fn has_video_fd(pid: u32) -> bool {
    let fd_dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let Ok(entries) = fs::read_dir(&fd_dir) else {
        return false;
    };

    for entry in entries.flatten() {
        if let Ok(target) = fs::read_link(entry.path()) {
            let target_str = target.to_string_lossy();
            if target_str.starts_with("/dev/video") {
                return true;
            }
        }
    }

    false
}
