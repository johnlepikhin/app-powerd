//! The set of processes an application owns, kept honest against `/proc`.

use crate::system::ProcessHandle;

/// A deduplicated set of process identities belonging to one application.
///
/// The daemon previously stored bare PIDs that were only ever appended to, so a
/// long-lived entry accumulated the PIDs of every process the application had
/// ever had, and kept signalling them long after they exited. Storing
/// identities instead makes "still ours" a question that can be answered, which
/// is what [`retain_alive`](Self::retain_alive) does.
#[derive(Debug, Default, Clone)]
pub struct ProcessSet {
    procs: Vec<ProcessHandle>,
    /// Whether this set ever held a live process.
    ///
    /// Distinguishes "everything died" from "we never learned a PID" — an
    /// application whose window carries no `_NET_WM_PID` starts out empty and
    /// must not be mistaken for one whose processes have all exited.
    had_live_procs: bool,
}

impl ProcessSet {
    /// Add a process identity, ignoring duplicates.
    pub fn insert(&mut self, handle: ProcessHandle) {
        if !self.procs.contains(&handle) {
            self.procs.push(handle);
            self.had_live_procs = true;
        }
    }

    /// Capture and add the identity of `pid`, if it is currently running.
    ///
    /// Returns the handle when the process was real, `None` when it could not be
    /// identified.
    pub fn insert_pid(&mut self, pid: u32) -> Option<ProcessHandle> {
        let handle = ProcessHandle::open(pid)?;
        self.insert(handle);
        Some(handle)
    }

    /// Whether any recorded process currently carries this PID.
    ///
    /// Matches on the PID alone, so it also answers "is this PID one of ours?"
    /// for a handle whose start time has since been recycled.
    pub fn contains_pid(&self, pid: u32) -> bool {
        self.procs.iter().any(|h| h.pid == pid)
    }

    /// The recorded process identities, in insertion order.
    pub fn handles(&self) -> &[ProcessHandle] {
        &self.procs
    }

    /// The PIDs of the recorded processes, in insertion order.
    pub fn pids(&self) -> Vec<u32> {
        self.procs.iter().map(|h| h.pid).collect()
    }

    /// Whether the set currently holds no process.
    ///
    /// Says nothing on its own about whether the application is gone — pair it
    /// with [`had_live_procs`](Self::had_live_procs).
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// Whether this set ever contained a live process.
    ///
    /// Only a set that *became* empty means the application is gone; one that
    /// was never populated says nothing about it.
    pub fn had_live_procs(&self) -> bool {
        self.had_live_procs
    }

    /// Drop the PIDs named in `pids`. Used to apply the `gone` list of an
    /// [`ApplyReport`](crate::system::apply::ApplyReport) at the moment a signal
    /// reveals a process has exited, without waiting for the next sweep.
    pub fn remove_pids(&mut self, pids: &[u32]) {
        self.procs.retain(|h| !pids.contains(&h.pid));
    }

    /// Drop every process that is no longer alive, returning those removed.
    ///
    /// A recycled PID counts as dead: its `starttime` differs, so the handle no
    /// longer names the process we were tracking.
    pub fn retain_alive(&mut self) -> Vec<ProcessHandle> {
        let mut reaped = Vec::new();
        self.procs.retain(|handle| {
            if handle.is_alive() {
                true
            } else {
                reaped.push(*handle);
                false
            }
        });
        reaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(pid: u32, starttime: u64) -> ProcessHandle {
        ProcessHandle { pid, starttime }
    }

    #[test]
    fn insert_deduplicates() {
        let mut set = ProcessSet::default();
        set.insert(handle(1, 10));
        set.insert(handle(1, 10));
        set.insert(handle(2, 11));
        assert_eq!(set.pids(), vec![1, 2]);
    }

    /// Same PID, different start time is a *different* process and must be kept
    /// separately rather than silently deduplicated onto the old one.
    #[test]
    fn same_pid_different_starttime_is_distinct() {
        let mut set = ProcessSet::default();
        set.insert(handle(1, 10));
        set.insert(handle(1, 99));
        assert_eq!(set.handles().len(), 2);
    }

    #[test]
    fn empty_set_never_populated_reports_no_live_procs() {
        let set = ProcessSet::default();
        assert!(set.is_empty());
        assert!(!set.had_live_procs());
    }

    #[test]
    fn retain_alive_drops_synthetic_handles() {
        let mut set = ProcessSet::default();
        // A synthetic starttime cannot match any real process.
        set.insert(handle(std::process::id(), u64::MAX));
        let reaped = set.retain_alive();
        assert_eq!(reaped.len(), 1);
        assert!(set.is_empty());
        // The set *did* hold something once, which is what marks the app gone.
        assert!(set.had_live_procs());
    }

    #[test]
    fn retain_alive_keeps_live_process() {
        let mut set = ProcessSet::default();
        set.insert_pid(std::process::id()).expect("own pid");
        assert!(set.retain_alive().is_empty());
        assert!(!set.is_empty());
    }

    #[test]
    fn remove_pids_drops_named_entries() {
        let mut set = ProcessSet::default();
        set.insert(handle(1, 10));
        set.insert(handle(2, 11));
        set.remove_pids(&[1]);
        assert_eq!(set.pids(), vec![2]);
    }
}
