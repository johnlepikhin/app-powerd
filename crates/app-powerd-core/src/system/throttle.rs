use std::path::Path;

use nix::libc;
use tracing::{debug, warn};

use super::cgroup::CgroupManager;
use crate::config::ThrottleParams;
use crate::error::SystemError;

/// Apply throttle policy to an application.
pub(crate) fn apply_throttle(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
    params: &ThrottleParams,
) -> Result<(), SystemError> {
    // Apply nice to all PIDs
    if let Some(nice_val) = params.nice {
        for &pid in pids {
            set_nice(pid, nice_val)?;
        }
    }

    // Apply cgroup CPU controls if available
    if let Some(path) = cgroup_path {
        if let Some(weight) = params.cpu_weight {
            cgroup_mgr.set_cpu_weight(path, weight)?;
        }
        if let Some(ref quota) = params.cpu_quota {
            cgroup_mgr.set_cpu_max(path, quota)?;
        }
    }

    Ok(())
}

/// Remove throttle policy from an application.
pub(crate) fn remove_throttle(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
) -> Result<(), SystemError> {
    // Reset nice to 0
    for &pid in pids {
        let _ = set_nice(pid, 0);
    }

    // Reset cgroup CPU controls
    if let Some(path) = cgroup_path {
        cgroup_mgr.reset_cpu(path)?;
    }

    Ok(())
}

fn set_nice(pid: u32, nice: i32) -> Result<(), SystemError> {
    if pid == 0 {
        return Err(SystemError::ProcessNotFound { pid });
    }
    // SAFETY: pid > 0 verified above; PRIO_PROCESS with valid pid is safe to call.
    // Kernel clamps nice to [-20, 19] range.
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        warn!(pid, nice, error = %err, "failed to set nice");
        return Err(SystemError::ThrottleFailed {
            app_id: format!("pid-{pid}"),
            reason: format!("setpriority failed: {err}"),
        });
    }
    debug!(pid, nice, "set nice");
    Ok(())
}
