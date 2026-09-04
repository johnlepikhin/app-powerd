//! Outcome of applying an operation to a set of processes.
//!
//! Freezing, thawing and throttling all fan out over many PIDs, and each PID can
//! end three different ways. Before this type each call site invented its own
//! policy — freeze was best-effort, throttle was fail-fast, un-throttle swallowed
//! everything — so the same failure meant different things depending on where it
//! happened. [`ApplyReport`] makes the three outcomes explicit and lets one
//! caller-side policy cover every operation.

use crate::error::SystemError;

use super::ProcessHandle;

/// What happened to a single process.
#[derive(Debug)]
pub(crate) enum PidOutcome {
    /// The operation was applied.
    Applied(ProcessHandle),
    /// The process no longer exists (`ESRCH`). Not an error — it needs nothing.
    Gone(u32),
    /// The operation failed for a reason that may or may not persist.
    Failed(u32, SystemError),
}

/// Aggregated outcome of an operation over a set of processes.
#[derive(Debug, Default)]
pub(crate) struct ApplyReport {
    /// Processes the operation actually reached. This is the authoritative set
    /// to record in the freeze journal — not what we intended to signal.
    ///
    /// For operations applied to a cgroup rather than to each PID (the freezer,
    /// the cpu controls), "reached" means the operation was applied to a group
    /// containing that process: there is no per-PID result to report, and the
    /// process is affected all the same. A process must not appear here when
    /// nothing was applied to it by either route — `nothing_applied` is what
    /// tells the caller the operation had no effect at all.
    pub applied: Vec<ProcessHandle>,
    /// Processes that had already exited. Callers drop these from their PID
    /// bookkeeping; that is what stops the daemon re-signalling dead PIDs.
    pub gone: Vec<u32>,
    /// Genuine failures, e.g. `EPERM` on a process owned by someone else.
    pub failed: Vec<(u32, SystemError)>,
}

impl ApplyReport {
    pub fn push(&mut self, outcome: PidOutcome) {
        match outcome {
            PidOutcome::Applied(handle) => self.applied.push(handle),
            PidOutcome::Gone(pid) => self.gone.push(pid),
            PidOutcome::Failed(pid, err) => self.failed.push((pid, err)),
        }
    }

    /// Collect the outcome of one PID, classifying `ESRCH` as `Gone`.
    pub fn record(&mut self, handle: ProcessHandle, result: Result<(), SystemError>) {
        self.push(match result {
            Ok(()) => PidOutcome::Applied(handle),
            Err(SystemError::ProcessGone { pid }) => PidOutcome::Gone(pid),
            Err(e) => PidOutcome::Failed(handle.pid, e),
        });
    }

    /// Nothing was reached. Distinct from "failed": an operation over a set of
    /// entirely dead processes applies to nothing yet went perfectly.
    pub fn nothing_applied(&self) -> bool {
        self.applied.is_empty()
    }

    /// Whether any process failed for a reason other than having exited.
    ///
    /// `gone` deliberately does not count: a dead process is the expected
    /// steady state for a long-running daemon, and treating it as an error is
    /// exactly what produced 62 641 log lines for a single closed application.
    pub fn had_real_error(&self) -> bool {
        !self.failed.is_empty()
    }

    /// PIDs that no longer need any follow-up: either handled or dead.
    pub fn settled_pids(&self) -> Vec<u32> {
        self.applied
            .iter()
            .map(|h| h.pid)
            .chain(self.gone.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(pid: u32) -> ProcessHandle {
        ProcessHandle { pid, starttime: 1 }
    }

    #[test]
    fn gone_is_not_an_error() {
        let mut report = ApplyReport::default();
        report.record(handle(7), Err(SystemError::ProcessGone { pid: 7 }));
        assert!(!report.had_real_error());
        assert!(report.nothing_applied());
        assert_eq!(report.gone, vec![7]);
    }

    #[test]
    fn failure_is_an_error() {
        let mut report = ApplyReport::default();
        report.record(handle(9), Err(SystemError::Nix(nix::errno::Errno::EPERM)));
        assert!(report.had_real_error());
        assert!(report.nothing_applied());
    }

    #[test]
    fn applied_clears_nothing_applied() {
        let mut report = ApplyReport::default();
        report.record(handle(3), Ok(()));
        report.record(handle(4), Err(SystemError::ProcessGone { pid: 4 }));
        assert!(!report.nothing_applied());
        assert!(!report.had_real_error());
        let mut settled = report.settled_pids();
        settled.sort_unstable();
        assert_eq!(settled, vec![3, 4]);
    }
}
