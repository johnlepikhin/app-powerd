use std::path::Path;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::{debug, warn};

use super::cgroup::CgroupManager;
use crate::error::SystemError;

/// Freeze an application's processes.
pub(crate) fn freeze_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
) -> Result<(), SystemError> {
    // Try cgroup freeze first
    if let Some(path) = cgroup_path {
        match cgroup_mgr.freeze(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(cgroup = %path.display(), error = %e, "cgroup freeze failed, falling back to SIGSTOP")
            }
        }
    }

    // Fallback: SIGSTOP all PIDs including descendants
    let all_pids = collect_all_pids(pids);
    let mut successes = 0u32;
    let mut last_error = None;
    for &pid in &all_pids {
        match signal_stop(pid) {
            Ok(()) => successes += 1,
            Err(e) => {
                warn!(pid, error = %e, "SIGSTOP failed");
                last_error = Some(e);
            }
        }
    }
    if successes == 0 {
        if let Some(e) = last_error {
            return Err(e);
        }
    }
    Ok(())
}

/// Thaw (unfreeze) an application's processes.
pub(crate) fn thaw_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    pids: &[u32],
) -> Result<(), SystemError> {
    if let Some(path) = cgroup_path {
        match cgroup_mgr.thaw(path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(cgroup = %path.display(), error = %e, "cgroup thaw failed, falling back to SIGCONT")
            }
        }
    }

    // Fallback: SIGCONT all PIDs including descendants
    let all_pids = collect_all_pids(pids);
    let mut successes = 0u32;
    let mut last_error = None;
    for &pid in &all_pids {
        match signal_cont(pid) {
            Ok(()) => successes += 1,
            Err(e) => {
                warn!(pid, error = %e, "SIGCONT failed");
                last_error = Some(e);
            }
        }
    }
    if successes == 0 {
        if let Some(e) = last_error {
            return Err(e);
        }
    }
    Ok(())
}

fn pid_to_nix(pid: u32) -> Result<Pid, SystemError> {
    let raw: i32 = pid
        .try_into()
        .map_err(|_| SystemError::ProcessNotFound { pid })?;
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
