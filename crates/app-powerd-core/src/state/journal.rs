//! On-disk record of every process the daemon has suspended.
//!
//! `SIGSTOP` is not reversible from outside the daemon: if the process that sent
//! it dies, nothing else knows the stopped processes exist. A crashed or killed
//! daemon therefore used to leave the whole session frozen — browsers,
//! messengers, language servers — with no way back short of the user finding
//! them by hand.
//!
//! The journal is the missing durable half of that operation. It is written
//! **before** the first signal goes out and cleared **after** the last one comes
//! back, so any state in between is recoverable:
//!
//! ```text
//! freeze: arm(planned) → freeze_app → commit(applied)
//! thaw:   thaw_app → retire(thawed ∪ gone)
//! ```
//!
//! Writing after the fact would leave a window in which processes are stopped
//! and unrecorded — precisely the failure the journal exists to prevent.
//!
//! Entries store [`ProcessHandle`]s rather than bare PIDs, so recovery can tell
//! "the process we froze" from "whatever inherited its PID since".

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::JournalError;
use crate::system::ProcessHandle;

/// Schema version of the journal file.
///
/// Bumped only for changes older daemons cannot safely interpret. Additive
/// fields must use `#[serde(default)]` and keep this constant unchanged.
const JOURNAL_VERSION: u32 = 1;

/// How an application was suspended, which determines how to release it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreezeMethod {
    /// cgroup v2 freezer.
    Cgroup,
    /// SIGSTOP to each process.
    Signal,
    /// Written by a newer daemon and not understood here.
    ///
    /// Recovery still sends `SIGCONT` for such entries: releasing a process that
    /// was not stopped is harmless, failing to release one is not.
    #[serde(other)]
    Unknown,
}

/// One suspended application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub app_id: String,
    pub method: FreezeMethod,
    #[serde(default)]
    pub cgroup_path: Option<PathBuf>,
    /// The processes actually signalled — including descendants, which is what
    /// makes this set larger and longer-lived than the registry's root PIDs.
    pub procs: Vec<ProcessHandle>,
}

/// Versioned wrapper written to disk.
#[derive(Debug, Serialize, Deserialize)]
struct JournalFile {
    version: u32,
    entries: Vec<JournalEntry>,
}

/// Result of restoring state left behind by a previous daemon run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Processes released with `SIGCONT`.
    pub thawed: usize,
    /// Recorded processes that had already exited.
    pub stale: usize,
    /// Recorded processes skipped because they are no longer ours.
    pub not_owned: usize,
}

impl RecoveryReport {
    /// Whether recovery found nothing at all to act on.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The set of currently suspended applications, mirrored to disk.
#[derive(Debug)]
pub struct FreezeJournal {
    path: PathBuf,
    entries: HashMap<String, JournalEntry>,
}

impl FreezeJournal {
    /// Path of the journal file for the current user.
    pub fn default_path() -> PathBuf {
        crate::system::runtime_dir()
            .join("app-powerd")
            .join("frozen.json")
    }

    /// Open the journal at `path`, loading any existing content.
    ///
    /// A corrupt or future-versioned file is an error and is **left on disk**:
    /// it may still describe stopped processes, and deleting it would destroy
    /// the only record of them.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the file exists but cannot be read, is not
    /// valid JSON, or carries a schema version this daemon does not understand.
    /// A missing file is not an error — it means nothing is suspended.
    pub fn load(path: PathBuf) -> Result<Self, JournalError> {
        let entries = match read_file(&path)? {
            Some(file) => file
                .entries
                .into_iter()
                .map(|entry| (entry.app_id.clone(), entry))
                .collect(),
            None => HashMap::new(),
        };
        Ok(Self { path, entries })
    }

    /// An empty in-memory journal that is never written to disk.
    ///
    /// Used by tests and by any embedder that does not want persistence.
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            entries: HashMap::new(),
        }
    }

    fn is_disabled(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// Whether no application is currently recorded as suspended.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate over the recorded applications, in unspecified order.
    pub fn entries(&self) -> impl Iterator<Item = &JournalEntry> {
        self.entries.values()
    }

    /// Record an intent to suspend, **before** any signal is sent.
    ///
    /// Failure here must abort the freeze: suspending processes we cannot record
    /// recreates exactly the unrecoverable state this module exists to prevent.
    /// Processes already recorded for this application are kept, not replaced.
    /// A previous thaw may have failed for some of them, and [`retire`] leaves
    /// exactly those behind; overwriting the record would drop the processes
    /// that are still stopped — a leftover helper reparented to init falls out
    /// of the newly expanded tree, and nothing would ever release it again.
    ///
    /// [`retire`]: Self::retire
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the record cannot be written to disk. The
    /// caller must treat this as fatal to the freeze and send no signals.
    pub fn arm(
        &mut self,
        app_id: &str,
        method: FreezeMethod,
        cgroup_path: Option<PathBuf>,
        planned: &[ProcessHandle],
    ) -> Result<(), JournalError> {
        let mut procs = self
            .entries
            .get(app_id)
            .map(|entry| entry.procs.clone())
            .unwrap_or_default();
        for &handle in planned {
            if !procs.contains(&handle) {
                procs.push(handle);
            }
        }

        self.entries.insert(
            app_id.to_string(),
            JournalEntry {
                app_id: app_id.to_string(),
                method,
                cgroup_path,
                procs,
            },
        );
        self.persist()
    }

    /// Narrow a record to the processes actually reached.
    ///
    /// Only the processes this operation intended to touch are reconsidered:
    /// `planned` minus `applied` is dropped, everything else the entry holds is
    /// left alone. Replacing the record with `applied` outright would discard
    /// processes carried over from an earlier partial thaw, which are precisely
    /// the ones still stopped.
    ///
    /// An application with no record is not an error: there is nothing to
    /// narrow.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the narrowed record cannot be written to
    /// disk. The in-memory record is updated regardless, so the on-disk copy
    /// stays a superset — it may name processes that were never reached, which
    /// costs a harmless `SIGCONT` at recovery.
    pub fn commit(
        &mut self,
        app_id: &str,
        planned: &[ProcessHandle],
        applied: &[ProcessHandle],
    ) -> Result<(), JournalError> {
        let Some(entry) = self.entries.get_mut(app_id) else {
            return Ok(());
        };

        entry
            .procs
            .retain(|handle| applied.contains(handle) || !planned.contains(handle));

        if entry.procs.is_empty() {
            debug!(app_id, "journal: nothing left recorded, dropping entry");
            self.entries.remove(app_id);
        }
        self.persist()
    }

    /// Remove processes that no longer need releasing.
    ///
    /// Works per process, not per application: if a thaw only partially
    /// succeeded, the processes still stopped must stay recorded. Dropping the
    /// whole entry would discard exactly the ones that still need help.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the updated record cannot be written to
    /// disk. The released processes stay recorded on disk in that case, which
    /// only costs a redundant `SIGCONT` should recovery ever run.
    pub fn retire(&mut self, app_id: &str, done: &[u32]) -> Result<(), JournalError> {
        let Some(entry) = self.entries.get_mut(app_id) else {
            return Ok(());
        };
        entry.procs.retain(|handle| !done.contains(&handle.pid));
        if entry.procs.is_empty() {
            self.entries.remove(app_id);
        }
        self.persist()
    }

    /// Drop processes that have exited, and entries left with none.
    ///
    /// Returns the number of entries removed. This is what eventually retires
    /// the record of an application whose registry entry disappeared while some
    /// of its descendants were still stopped.
    pub fn sweep_dead(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, entry| {
            entry.procs.retain(|handle| handle.is_alive());
            !entry.procs.is_empty()
        });
        let removed = before - self.entries.len();
        if removed > 0 {
            if let Err(e) = self.persist() {
                warn!(error = %e, "journal: failed to persist after sweep");
            }
        }
        removed
    }

    /// Every process currently recorded as suspended.
    pub fn all_procs(&self) -> Vec<ProcessHandle> {
        self.entries
            .values()
            .flat_map(|entry| entry.procs.iter().copied())
            .collect()
    }

    /// Delete the journal file and forget everything.
    ///
    /// Only safe when nothing can still be stopped; use [`retire`] whenever
    /// some processes may remain suspended.
    ///
    /// [`retire`]: Self::retire
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the file exists but cannot be removed. An
    /// already-absent file is not an error.
    pub fn clear(&mut self) -> Result<(), JournalError> {
        self.entries.clear();
        if self.is_disabled() {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(JournalError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    /// Write the current contents atomically.
    ///
    /// A temporary file in the same directory followed by `rename` means a
    /// reader never sees a half-written journal, and `sync_all` means the
    /// content is on disk before we claim it is.
    fn persist(&self) -> Result<(), JournalError> {
        if self.is_disabled() {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(|source| JournalError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        // The journal names processes of this user only; nobody else needs it.
        if let Err(e) = fs::set_permissions(dir, fs::Permissions::from_mode(0o700)) {
            debug!(
                path = %dir.display(),
                error = %e,
                "journal: could not restrict directory permissions"
            );
        }

        let file = JournalFile {
            version: JOURNAL_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|source| JournalError::Corrupt {
            path: self.path.clone(),
            source,
        })?;

        let tmp = self.path.with_extension("json.tmp");
        let io_err = |path: &Path| {
            let path = path.to_path_buf();
            move |source| JournalError::Io { path, source }
        };

        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io_err(&tmp))?;
        // `mode` above applies only when the file is created, and is masked by
        // umask even then; the temporary file is reused across calls, so set the
        // mode explicitly on every write.
        handle
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io_err(&tmp))?;
        handle.write_all(&json).map_err(io_err(&tmp))?;
        handle.sync_all().map_err(io_err(&tmp))?;
        drop(handle);

        fs::rename(&tmp, &self.path).map_err(io_err(&self.path))?;
        Ok(())
    }

    /// Release everything recorded by a previous daemon run, then delete the
    /// journal.
    ///
    /// Standalone by design: it needs no engine, no display server and no
    /// running daemon, so the same code serves both daemon startup and the
    /// emergency `thaw-all` command.
    ///
    /// That is why it writes to cgroupfs and signals processes itself instead of
    /// going through [`CgroupManager`](crate::system::CgroupManager): the manager
    /// belongs to a live daemon that has detected its cgroup capabilities, and
    /// recovery runs precisely when no such daemon exists. The journal already
    /// records the method and path used, so nothing needs to be re-detected.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError`] if the journal exists but cannot be read,
    /// parsed, or was written by a newer schema version. Failures to release an
    /// individual process are logged, not returned: one unrecoverable process
    /// must not stop the rest from being released.
    pub fn recover(path: &Path) -> Result<RecoveryReport, JournalError> {
        let Some(file) = read_file(path)? else {
            return Ok(RecoveryReport::default());
        };

        let mut report = RecoveryReport::default();
        for entry in &file.entries {
            // A cgroup path from a previous boot need not exist any more; thaw
            // it when it does and fall through to signals regardless, because
            // an unnecessary SIGCONT costs nothing and a missed one is
            // unrecoverable.
            if let Some(cgroup) = &entry.cgroup_path {
                let freeze_file = cgroup.join("cgroup.freeze");
                if let Err(e) = fs::write(&freeze_file, "0") {
                    debug!(
                        path = %freeze_file.display(),
                        app_id = %entry.app_id,
                        error = %e,
                        "journal: could not thaw cgroup, falling back to signals"
                    );
                }
            }
            for handle in &entry.procs {
                if !handle.is_alive() {
                    report.stale += 1;
                    continue;
                }
                if !handle.is_owned() {
                    warn!(
                        pid = handle.pid,
                        app_id = %entry.app_id,
                        "journal: recorded process is not owned by this user, skipping"
                    );
                    report.not_owned += 1;
                    continue;
                }
                match crate::system::freeze::signal_cont(handle.pid) {
                    Ok(()) => report.thawed += 1,
                    Err(e) => {
                        warn!(pid = handle.pid, error = %e, "journal: recovery thaw failed")
                    }
                }
            }
        }

        if !report.is_empty() {
            info!(
                thawed = report.thawed,
                stale = report.stale,
                not_owned = report.not_owned,
                "recovered suspended processes from a previous run"
            );
        }

        if let Err(e) = fs::remove_file(path) {
            warn!(
                path = %path.display(),
                error = %e,
                "journal: could not clear after recovery"
            );
        }
        Ok(report)
    }
}

/// Read and validate the journal file. `Ok(None)` means "no journal yet".
fn read_file(path: &Path) -> Result<Option<JournalFile>, JournalError> {
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(JournalError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    let file: JournalFile =
        serde_json::from_slice(&raw).map_err(|source| JournalError::Corrupt {
            path: path.to_path_buf(),
            source,
        })?;

    if file.version != JOURNAL_VERSION {
        return Err(JournalError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: file.version,
            expected: JOURNAL_VERSION,
        });
    }

    Ok(Some(file))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("app-powerd-journal-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        dir.join("frozen.json")
    }

    fn handle(pid: u32, starttime: u64) -> ProcessHandle {
        ProcessHandle { pid, starttime }
    }

    #[test]
    fn arm_commit_roundtrip() {
        let path = temp_path("roundtrip");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        let planned = [handle(1, 10), handle(2, 11)];
        journal
            .arm("Firefox", FreezeMethod::Signal, None, &planned)
            .unwrap();
        journal
            .commit("Firefox", &planned, &[handle(1, 10)])
            .unwrap();

        let reloaded = FreezeJournal::load(path.clone()).unwrap();
        let entry = reloaded.entries().next().unwrap();
        assert_eq!(entry.app_id, "Firefox");
        assert_eq!(entry.procs, vec![handle(1, 10)]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A process left behind by a failed thaw must survive the next freeze of
    /// the same application. It is still stopped, and it may well have dropped
    /// out of the process tree by then, so the journal is the only thing that
    /// still knows about it.
    #[test]
    fn arm_keeps_processes_left_by_an_earlier_partial_thaw() {
        let path = temp_path("arm-merge");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm(
                "Chrome",
                FreezeMethod::Signal,
                None,
                &[handle(1, 10), handle(2, 11)],
            )
            .unwrap();
        // Thaw released only pid 1; pid 2 stays recorded as still stopped.
        journal.retire("Chrome", &[1]).unwrap();

        // The next freeze sees a different tree that no longer contains pid 2.
        let planned = [handle(3, 12)];
        journal
            .arm("Chrome", FreezeMethod::Signal, None, &planned)
            .unwrap();
        journal.commit("Chrome", &planned, &planned).unwrap();

        let reloaded = FreezeJournal::load(path.clone()).unwrap();
        let procs = &reloaded.entries().next().expect("entry survives").procs;
        assert!(
            procs.contains(&handle(2, 11)),
            "the process left stopped by the failed thaw must still be recorded, got {procs:?}"
        );
        assert!(procs.contains(&handle(3, 12)));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// `commit` narrows only what this operation planned; unrelated carry-over
    /// must not be swept away by a freeze that failed to reach it.
    #[test]
    fn commit_only_narrows_planned_processes() {
        let path = temp_path("commit-narrow");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm("App", FreezeMethod::Signal, None, &[handle(9, 90)])
            .unwrap();
        journal.retire("App", &[]).unwrap();

        let planned = [handle(1, 10), handle(2, 11)];
        journal
            .arm("App", FreezeMethod::Signal, None, &planned)
            .unwrap();
        // Only pid 1 was reached this time.
        journal.commit("App", &planned, &[handle(1, 10)]).unwrap();

        let reloaded = FreezeJournal::load(path.clone()).unwrap();
        let procs = &reloaded.entries().next().unwrap().procs;
        assert!(procs.contains(&handle(9, 90)), "carry-over kept");
        assert!(procs.contains(&handle(1, 10)), "applied kept");
        assert!(
            !procs.contains(&handle(2, 11)),
            "planned-but-unreached dropped"
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// Partial thaw must leave the still-stopped processes recorded — dropping
    /// the whole entry would discard exactly the ones that still need help.
    #[test]
    fn retire_keeps_processes_that_were_not_released() {
        let path = temp_path("partial-retire");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm(
                "Chrome",
                FreezeMethod::Signal,
                None,
                &[handle(1, 10), handle(2, 11)],
            )
            .unwrap();
        journal.retire("Chrome", &[1]).unwrap();

        let reloaded = FreezeJournal::load(path.clone()).unwrap();
        let entry = reloaded.entries().next().expect("entry must survive");
        assert_eq!(entry.procs, vec![handle(2, 11)]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn retire_removes_entry_when_all_released() {
        let path = temp_path("full-retire");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm("Chrome", FreezeMethod::Signal, None, &[handle(1, 10)])
            .unwrap();
        journal.retire("Chrome", &[1]).unwrap();
        assert!(FreezeJournal::load(path.clone()).unwrap().is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A corrupt journal may still describe stopped processes, so it must be an
    /// error and the file must survive for a human to inspect.
    #[test]
    fn corrupt_file_errors_and_is_preserved() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ this is not json").unwrap();

        assert!(matches!(
            FreezeJournal::load(path.clone()),
            Err(JournalError::Corrupt { .. })
        ));
        assert!(matches!(
            FreezeJournal::recover(&path),
            Err(JournalError::Corrupt { .. })
        ));
        assert!(path.exists(), "corrupt journal must not be deleted");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_version_errors_and_is_preserved() {
        let path = temp_path("version");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"version":99,"entries":[]}"#).unwrap();

        assert!(matches!(
            FreezeJournal::load(path.clone()),
            Err(JournalError::UnsupportedVersion { found: 99, .. })
        ));
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// An unrecognised freeze method must not break parsing: recovery still
    /// knows how to send SIGCONT.
    #[test]
    fn unknown_method_parses_as_unknown() {
        let path = temp_path("method");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"version":1,"entries":[{"app_id":"X","method":"quantum","procs":[]}]}"#,
        )
        .unwrap();

        let journal = FreezeJournal::load(path.clone()).unwrap();
        assert_eq!(
            journal.entries().next().unwrap().method,
            FreezeMethod::Unknown
        );
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    /// A recorded PID whose start time no longer matches belongs to somebody
    /// else now and must never be signalled.
    #[test]
    fn recover_skips_recycled_pids() {
        let path = temp_path("recycled");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm(
                "Ghost",
                FreezeMethod::Signal,
                None,
                &[handle(std::process::id(), u64::MAX)],
            )
            .unwrap();

        let report = FreezeJournal::recover(&path).unwrap();
        assert_eq!(report.thawed, 0);
        assert_eq!(report.stale, 1);
        assert!(!path.exists(), "successful recovery clears the journal");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn recover_on_missing_file_is_a_noop() {
        let path = temp_path("missing");
        assert_eq!(
            FreezeJournal::recover(&path).unwrap(),
            RecoveryReport::default()
        );
    }

    #[test]
    fn sweep_drops_entries_whose_processes_all_died() {
        let path = temp_path("sweep");
        let mut journal = FreezeJournal::load(path.clone()).unwrap();
        journal
            .arm(
                "Dead",
                FreezeMethod::Signal,
                None,
                &[handle(std::process::id(), u64::MAX)],
            )
            .unwrap();
        assert_eq!(journal.sweep_dead(), 1);
        assert!(journal.is_empty());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
