use std::path::Path;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, warn};

use super::cgroup::CgroupManager;
use crate::error::SystemError;

/// Freeze an application's processes.
pub fn freeze_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
) -> Result<(), SystemError> {
    // Try cgroup freeze first
    if let Some(path) = cgroup_path {
        if cgroup_mgr.freeze(path).is_ok() {
            return Ok(());
        }
        warn!("cgroup freeze failed, falling back to SIGSTOP");
    }

    // Fallback: SIGSTOP all PIDs including descendants
    let all_pids = collect_all_pids(pids);
    for &pid in &all_pids {
        if let Err(e) = signal_stop(pid) {
            warn!(pid, error = %e, "SIGSTOP failed");
        }
    }
    Ok(())
}

/// Thaw (unfreeze) an application's processes.
pub fn thaw_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
) -> Result<(), SystemError> {
    if let Some(path) = cgroup_path {
        if cgroup_mgr.thaw(path).is_ok() {
            return Ok(());
        }
        warn!("cgroup thaw failed, falling back to SIGCONT");
    }

    // Fallback: SIGCONT all PIDs including descendants
    let all_pids = collect_all_pids(pids);
    for &pid in &all_pids {
        if let Err(e) = signal_cont(pid) {
            warn!(pid, error = %e, "SIGCONT failed");
        }
    }
    Ok(())
}

fn pid_to_nix(pid: u32) -> Result<Pid, SystemError> {
    let raw: i32 = pid.try_into().map_err(|_| SystemError::ProcessNotFound { pid })?;
    if raw <= 0 {
        return Err(SystemError::ProcessNotFound { pid });
    }
    Ok(Pid::from_raw(raw))
}

/// Collect given PIDs plus all their descendant PIDs.
fn collect_all_pids(pids: &[u32]) -> Vec<u32> {
    let mut all = pids.to_vec();
    for &pid in pids {
        all.extend(super::process::descendant_pids(pid));
    }
    all
}

fn signal_stop(pid: u32) -> Result<(), SystemError> {
    debug!(pid, "sending SIGSTOP");
    signal::kill(pid_to_nix(pid)?, Signal::SIGSTOP)?;
    Ok(())
}

fn signal_cont(pid: u32) -> Result<(), SystemError> {
    debug!(pid, "sending SIGCONT");
    signal::kill(pid_to_nix(pid)?, Signal::SIGCONT)?;
    Ok(())
}
