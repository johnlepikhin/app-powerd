use std::path::Path;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use tracing::debug;

use super::apply::ApplyReport;
use super::cgroup::CgroupManager;
use super::ProcessHandle;
use crate::error::SystemError;

/// Freeze an application's processes.
///
/// `procs` is the complete, already-expanded and already-filtered set the caller
/// wants stopped. Expansion happens in the caller because it costs a `/proc`
/// walk that must not run on the engine's single-threaded event loop, and
/// because only the caller knows which descendants protection rules exclude.
pub(crate) fn freeze_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    procs: &[ProcessHandle],
) -> ApplyReport {
    if let Some(path) = cgroup_path {
        if cgroup_mgr.supports_freezer() {
            match cgroup_mgr.freeze(path) {
                Ok(()) => return group_report(procs),
                Err(e) => {
                    debug!(cgroup = %path.display(), error = %e, "cgroup freeze failed, using signals")
                }
            }
        }
    }

    apply_signal(procs, signal_stop)
}

/// Outcome of an operation applied to a cgroup as a whole.
///
/// The freezer acts on the group, so every live member is genuinely covered.
/// Processes that have already exited are still reported as `Gone` rather than
/// `Applied`, so callers drop them from their bookkeeping instead of carrying
/// corpses in the freeze journal until the next sweep.
fn group_report(procs: &[ProcessHandle]) -> ApplyReport {
    let mut report = ApplyReport::default();
    for &handle in procs {
        if handle.is_alive() {
            report.push(super::apply::PidOutcome::Applied(handle));
        } else {
            report.push(super::apply::PidOutcome::Gone(handle.pid));
        }
    }
    report
}

/// Thaw (unfreeze) an application's processes.
pub(crate) fn thaw_app(
    cgroup_mgr: &CgroupManager,
    cgroup_path: Option<&Path>,
    procs: &[ProcessHandle],
) -> ApplyReport {
    // Unlike freezing, thawing is attempted through *both* paths when a cgroup
    // is involved: a process may have been stopped by signal before the cgroup
    // existed, and a redundant SIGCONT is harmless while a missed one is not.
    if let Some(path) = cgroup_path {
        if cgroup_mgr.supports_freezer() {
            if let Err(e) = cgroup_mgr.thaw(path) {
                debug!(cgroup = %path.display(), error = %e, "cgroup thaw failed, using signals");
            }
        }
    }

    apply_signal(procs, signal_cont)
}

/// Send a signal to every process, classifying each outcome.
fn apply_signal(procs: &[ProcessHandle], send: fn(u32) -> Result<(), SystemError>) -> ApplyReport {
    let mut report = ApplyReport::default();
    for &handle in procs {
        // Re-check identity immediately before signalling: between expansion and
        // here the process may have exited and its PID been reused, and
        // stopping a stranger is worse than doing nothing.
        if !handle.is_alive() {
            report.push(super::apply::PidOutcome::Gone(handle.pid));
            continue;
        }
        report.record(handle, send(handle.pid));
    }
    report
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

/// Map a signalling errno to a domain error.
///
/// `ESRCH` is translated at the call site rather than in `From<Errno>` because
/// only here is the PID known, and the PID is the whole point: callers use it to
/// drop the process from their bookkeeping.
fn classify(pid: u32, result: nix::Result<()>) -> Result<(), SystemError> {
    match result {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::ESRCH) => Err(SystemError::ProcessGone { pid }),
        Err(e) => Err(SystemError::Nix(e)),
    }
}

pub(crate) fn signal_stop(pid: u32) -> Result<(), SystemError> {
    debug!(pid, "sending SIGSTOP");
    classify(pid, signal::kill(pid_to_nix(pid)?, Signal::SIGSTOP))
}

pub(crate) fn signal_cont(pid: u32) -> Result<(), SystemError> {
    debug!(pid, "sending SIGCONT");
    classify(pid, signal::kill(pid_to_nix(pid)?, Signal::SIGCONT))
}

/// Collect the given PIDs plus all their descendants, as process identities.
///
/// Blocking `/proc` walk — callers must run it off the event loop.
pub(crate) fn expand_tree(roots: &[u32]) -> Vec<ProcessHandle> {
    let mut handles = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &pid in roots {
        for candidate in std::iter::once(pid).chain(super::process::descendant_pids(pid)) {
            if seen.insert(candidate) {
                if let Some(handle) = ProcessHandle::open(candidate) {
                    handles.push(handle);
                }
            }
        }
    }
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_esrch_to_process_gone() {
        let err = classify(42, Err(nix::errno::Errno::ESRCH)).unwrap_err();
        assert!(matches!(err, SystemError::ProcessGone { pid: 42 }));
    }

    #[test]
    fn classify_preserves_other_errnos() {
        let err = classify(42, Err(nix::errno::Errno::EPERM)).unwrap_err();
        assert!(matches!(err, SystemError::Nix(nix::errno::Errno::EPERM)));
    }

    /// A stale handle must be reported as gone without a signal ever leaving the
    /// process — this is the guard against stopping a recycled PID.
    #[test]
    fn stale_handle_is_reported_gone_without_signalling() {
        let stale = ProcessHandle {
            pid: std::process::id(),
            starttime: u64::MAX,
        };
        let report = apply_signal(&[stale], |_| panic!("must not signal a stale handle"));
        assert_eq!(report.gone, vec![std::process::id()]);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn expand_tree_includes_self() {
        let handles = expand_tree(&[std::process::id()]);
        assert!(handles.iter().any(|h| h.pid == std::process::id()));
    }
}
