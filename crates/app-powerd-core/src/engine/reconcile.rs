//! Periodic reconciliation of the daemon's model with the actual process table.
//!
//! Removal used to be driven purely by window-close events. A missed
//! `DestroyNotify` — or an application that exits without its window being seen
//! to close — left an entry that lived forever, re-signalling PIDs that no
//! longer existed every few seconds. Process liveness, not window bookkeeping,
//! is the honest source of truth for "is this application still here", and this
//! is where that truth is applied.

use super::*;

impl Engine {
    /// Start a reconcile sweep.
    ///
    /// The `/proc` reads happen on a blocking thread and come back as
    /// [`EngineEvent::ReconcileResult`]. The event loop is single-threaded and
    /// also serves IPC, focus events and every timer, so a synchronous sweep
    /// would stall all of them for as long as it took. The journal is swept in
    /// the same task — it records the expanded tree, so it holds more PIDs than
    /// the registry and checking it is the more expensive half; only the
    /// resulting file write stays on the loop.
    ///
    /// A tick is skipped while the previous sweep is still running: the interval
    /// bounds how often a sweep starts, not how long one takes.
    pub(crate) fn handle_reconcile(&mut self) {
        METRICS
            .reconcile_ticks_total
            .fetch_add(1, Ordering::Relaxed);

        if self.reconcile_in_flight {
            debug!("reconcile still running, skipping this tick");
            self.maybe_refresh_protection();
            return;
        }

        let snapshot: Vec<(AppId, Vec<ProcessHandle>)> = self
            .registry
            .iter()
            .map(|(id, entry)| (id.clone(), entry.roots().handles().to_vec()))
            .collect();
        let journal_snapshot: Vec<(String, Vec<ProcessHandle>)> = self
            .journal
            .entries()
            .map(|entry| (entry.app_id.clone(), entry.procs.clone()))
            .collect();

        if snapshot.is_empty() && journal_snapshot.is_empty() {
            self.maybe_refresh_protection();
            return;
        }

        self.reconcile_in_flight = true;
        let tx = self.event_tx.clone();
        // The result event is sent even when the blocking task panics, because
        // it is also what clears `reconcile_in_flight`; a lost event would stop
        // reconciliation for the lifetime of the daemon.
        tokio::spawn(async move {
            let sweep = tokio::task::spawn_blocking(move || {
                let reaped: Vec<(AppId, Vec<ProcessHandle>)> = snapshot
                    .into_iter()
                    .map(|(app_id, handles)| {
                        let dead: Vec<ProcessHandle> = handles
                            .into_iter()
                            .filter(|handle| !handle.is_alive())
                            .collect();
                        (app_id, dead)
                    })
                    .filter(|(_, dead)| !dead.is_empty())
                    .collect();
                let journal_dead: Vec<(String, Vec<u32>)> = journal_snapshot
                    .into_iter()
                    .map(|(app_id, handles)| {
                        let dead: Vec<u32> = handles
                            .into_iter()
                            .filter(|handle| !handle.is_alive())
                            .map(|handle| handle.pid)
                            .collect();
                        (app_id, dead)
                    })
                    .filter(|(_, dead)| !dead.is_empty())
                    .collect();
                (reaped, journal_dead)
            })
            .await;

            let (reaped, journal_dead) = match sweep {
                Ok(result) => result,
                Err(e) => {
                    warn!(error = %e, "reconcile sweep task failed");
                    (Vec::new(), Vec::new())
                }
            };
            let _ = tx
                .send(EngineEvent::ReconcileResult {
                    reaped,
                    journal_dead,
                })
                .await;
        });

        self.maybe_refresh_protection();
    }

    /// Apply the result of a sweep: drop dead PIDs, retire dead applications.
    pub(crate) fn apply_reconcile_result(
        &mut self,
        reaped: Vec<(AppId, Vec<ProcessHandle>)>,
        journal_dead: Vec<(String, Vec<u32>)>,
    ) {
        self.reconcile_in_flight = false;
        self.sweep_journal(journal_dead);

        for (app_id, dead) in reaped {
            let Some(entry) = self.registry.get_mut(&app_id) else {
                continue;
            };
            let dead_pids: Vec<u32> = dead.iter().map(|h| h.pid).collect();
            entry.roots_mut().remove_pids(&dead_pids);
            METRICS
                .pids_reaped_total
                .fetch_add(dead_pids.len() as u64, Ordering::Relaxed);
            debug!(app_id = %app_id, pids = ?dead_pids, "reaped exited processes");

            if entry.is_defunct() {
                self.retire_defunct_app(&app_id);
            }
        }
    }

    /// Remove an application whose processes have all exited.
    ///
    /// Its journal entry is deliberately left alone: descendants that were
    /// signalled may still be alive and stopped after the main process died —
    /// the browser helpers and language servers found stopped on the reference
    /// machine are exactly this case. The journal retires them on its own once
    /// they too are gone.
    fn retire_defunct_app(&mut self, app_id: &AppId) {
        info!(app_id = %app_id, "all processes exited, dropping tracked app");
        METRICS
            .apps_removed_stale_total
            .fetch_add(1, Ordering::Relaxed);

        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.cancel_all_timers();
        }
        let cgroup = self
            .registry
            .get(app_id)
            .and_then(|entry| entry.cgroup_path_buf());
        if let Some(path) = cgroup {
            if let Err(e) = self.cgroup_mgr.remove_cgroup(&path) {
                debug!(app_id = %app_id, error = %e, "failed to remove cgroup for defunct app");
            }
        }

        self.registry.remove_app(app_id);
        self.suspend_failures.remove(app_id);
        self.rate_limiter.forget_app(app_id.as_str());
    }

    /// Drop journal entries whose processes have all exited.
    ///
    /// Liveness was determined off the event loop; what happens here is the
    /// bookkeeping and the file write.
    fn sweep_journal(&mut self, journal_dead: Vec<(String, Vec<u32>)>) {
        if journal_dead.is_empty() {
            return;
        }
        let before = self.journal.entries().count();
        for (app_id, dead_pids) in journal_dead {
            if let Err(e) = self.journal.retire(&app_id, &dead_pids) {
                debug!(app_id, error = %e, "journal: failed to persist after sweep");
            }
        }
        let dropped = before.saturating_sub(self.journal.entries().count());
        if dropped > 0 {
            METRICS
                .journal_stale_dropped_total
                .fetch_add(dropped as u64, Ordering::Relaxed);
            debug!(dropped, "journal: retired entries with no live processes");
        }
    }

    /// Refresh the session-bus owner set if it is due.
    ///
    /// Runs on a blocking thread: zbus's blocking API is what this crate already
    /// uses, a session bus carries hundreds of names, and a hung bus would
    /// otherwise block the event loop for minutes.
    fn maybe_refresh_protection(&mut self) {
        if !self.dbus_check {
            return;
        }
        let due = self
            .last_dbus_refresh
            .is_none_or(|last| last.elapsed() >= self.dbus_refresh_interval);
        if !due {
            return;
        }
        self.last_dbus_refresh = Some(Instant::now());

        let tx = self.event_tx.clone();
        tokio::task::spawn_blocking(move || {
            match crate::system::protection::collect_dbus_owners(DBUS_SWEEP_BUDGET) {
                Ok(owners) => {
                    let _ = tx.blocking_send(EngineEvent::ProtectionRefreshed(owners));
                }
                // Keep the previous set on failure rather than clearing it:
                // an unreachable bus is no reason to stop protecting the
                // processes we already know about.
                Err(e) => debug!(error = %e, "protection: dbus sweep failed, keeping previous set"),
            }
        });
    }
}
