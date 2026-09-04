use std::path::Path;

use nix::libc;
use tracing::debug;

use super::apply::ApplyReport;
use super::cgroup::CgroupManager;
use super::ProcessHandle;
use crate::config::ThrottleParams;
use crate::error::SystemError;

/// Apply throttle policy to an application.
///
/// Best-effort per process, matching freeze. It used to abort on the first
/// failure, so a single exited PID meant the application was never throttled at
/// all — only retried every ten seconds, forever.
pub(crate) fn apply_throttle(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    procs: &[ProcessHandle],
    params: &ThrottleParams,
) -> ApplyReport {
    let mut report = ApplyReport::default();

    // CPU controls apply to the whole cgroup at once, so a failure here is not
    // attributable to any single PID; it is reported against the application.
    // Done first because whether any of them landed decides what the per-process
    // outcome below may claim.
    let mut cpu_applied = false;
    if let Some(path) = cgroup_path {
        if cgroup_mgr.supports_cpu_control() {
            if let Some(weight) = params.cpu_weight {
                match cgroup_mgr.set_cpu_weight(path, weight) {
                    Ok(()) => cpu_applied = true,
                    Err(e) => debug!(error = %e, "failed to set cpu.weight"),
                }
            }
            if let Some(ref quota) = params.cpu_quota {
                match cgroup_mgr.set_cpu_max(path, quota) {
                    Ok(()) => cpu_applied = true,
                    Err(e) => debug!(error = %e, "failed to set cpu.max"),
                }
            }
        }
    }

    for &handle in procs {
        if !handle.is_alive() {
            report.push(super::apply::PidOutcome::Gone(handle.pid));
            continue;
        }
        match params.nice {
            Some(nice_val) => report.record(handle, set_nice(handle.pid, nice_val)),
            // Without a nice component the process was reached only if a cgroup
            // CPU control actually landed on the group holding it. Claiming
            // otherwise would let an application move to `Throttled` — and bump
            // the throttle counter — on a system with no cgroup delegation,
            // where nothing at all was applied.
            None if cpu_applied => report.record(handle, Ok(())),
            None => {}
        }
    }

    report
}

/// Remove throttle policy from an application.
pub(crate) fn remove_throttle(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    procs: &[ProcessHandle],
) -> ApplyReport {
    let mut report = ApplyReport::default();
    for &handle in procs {
        if !handle.is_alive() {
            report.push(super::apply::PidOutcome::Gone(handle.pid));
            continue;
        }
        report.record(handle, set_nice(handle.pid, 0));
    }

    if let Some(path) = cgroup_path {
        if cgroup_mgr.supports_cpu_control() {
            cgroup_mgr.reset_cpu(path);
        }
    }

    report
}

fn set_nice(pid: u32, nice: i32) -> Result<(), SystemError> {
    if pid == 0 {
        return Err(SystemError::ProcessNotFound { pid });
    }
    // SAFETY: pid > 0 verified above; PRIO_PROCESS with a valid pid is safe to
    // call. The kernel clamps nice to [-20, 19].
    //
    // setpriority returns -1 both on error and, on some libc versions, for a
    // successful call, so errno must be cleared first and consulted after.
    nix::errno::Errno::clear();
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if ret == -1 {
        let errno = nix::errno::Errno::last();
        if errno != nix::errno::Errno::UnknownErrno {
            return Err(match errno {
                nix::errno::Errno::ESRCH => SystemError::ProcessGone { pid },
                other => SystemError::Nix(other),
            });
        }
    }
    debug!(pid, nice, "set nice");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_nice_on_missing_process_reports_gone() {
        // PID 0 is never a valid signal target here.
        assert!(matches!(
            set_nice(0, 5),
            Err(SystemError::ProcessNotFound { pid: 0 })
        ));
    }

    /// Raising our own niceness is permitted for any user, so this exercises the
    /// success path including the errno handling.
    #[test]
    fn set_nice_on_self_succeeds() {
        let current = unsafe { libc::getpriority(libc::PRIO_PROCESS, std::process::id()) };
        assert!(set_nice(std::process::id(), current).is_ok());
    }
}
