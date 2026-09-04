use super::*;

impl Engine {
    /// Carry out a state transition.
    ///
    /// `procs` is the fully expanded, protection-filtered set of processes the
    /// caller wants acted on. Passing it in keeps the `/proc` walk off the event
    /// loop and means the freeze path and the guard checks agree on exactly
    /// which processes are involved.
    ///
    /// The registry is left consistent on every path. Previously a failure
    /// returned early without updating the state, so an application that had not
    /// been frozen was still recorded as `Frozen` — and the shutdown thaw, which
    /// filters on state, then skipped or targeted the wrong entries.
    #[instrument(skip(self, procs))]
    pub(crate) fn execute_transition(
        &mut self,
        app_id: &AppId,
        new_state: AppState,
        action: TransitionAction,
        procs: &[ProcessHandle],
    ) {
        if !self.should_manage() && action.requires_management() {
            // Management is off, so nothing was applied. Recording the target
            // state anyway would be a lie the rest of the engine acts on.
            debug!(app_id = %app_id, "management disabled, skipping action");
            return;
        }

        let (cgroup_path, policy) = {
            let Some(entry) = self.registry.get(app_id) else {
                return;
            };
            (entry.cgroup_path_buf(), entry.policy().clone())
        };
        let cgroup_p = cgroup_path.as_deref();

        match action {
            TransitionAction::StartSuspendTimer => {
                let handle = Self::spawn_delayed_event(
                    &self.event_tx,
                    policy.suspend_delay,
                    EngineEvent::SuspendTimerFired {
                        app_id: app_id.clone(),
                    },
                );
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.set_suspend_timer(handle);
                }
            }
            TransitionAction::CancelSuspendTimer => {
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.cancel_suspend_timer();
                }
            }
            TransitionAction::ApplyThrottle => {
                let report = throttle::apply_throttle(
                    &self.cgroup_mgr,
                    cgroup_p,
                    procs,
                    &policy.throttle_params(),
                );
                self.absorb_gone(app_id, &report);
                if report.nothing_applied() {
                    self.on_suspend_failed(app_id, &report, "throttle");
                    return;
                }
                self.suspend_failures.remove(app_id);
                METRICS.apps_throttled_total.fetch_add(1, Ordering::Relaxed);
            }
            TransitionAction::ApplyFreeze => {
                if !self.freeze_with_journal(app_id, cgroup_path.clone(), procs) {
                    return;
                }
                if policy.maintenance_resume.enabled {
                    self.start_maintenance_timer(app_id);
                }
            }
            TransitionAction::RemoveThrottle | TransitionAction::Thaw => {
                let elapsed_ms = self
                    .registry
                    .get(app_id)
                    .map(|e| e.state_since().elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let current_state = if action == TransitionAction::Thaw {
                    AppState::Frozen
                } else {
                    AppState::Throttled
                };
                self.restore_app_resources(app_id, current_state, cgroup_p, procs, elapsed_ms);
            }
            TransitionAction::NoOp => {}
        }

        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_state(new_state);
        }
    }

    /// Freeze an application, recording the intent before the first signal.
    ///
    /// Returns whether the freeze went ahead. The journal write comes first so
    /// that a crash at any point afterwards still leaves a record of the stopped
    /// processes; if that write fails the freeze is abandoned rather than
    /// performed unrecorded, because unrecorded stopped processes are exactly
    /// the state nobody can recover from.
    fn freeze_with_journal(
        &mut self,
        app_id: &AppId,
        cgroup_path: Option<std::path::PathBuf>,
        procs: &[ProcessHandle],
    ) -> bool {
        if procs.is_empty() {
            debug!(app_id = %app_id, "no live processes to freeze");
            return false;
        }

        let method = if cgroup_path.is_some() && self.cgroup_mgr.supports_freezer() {
            FreezeMethod::Cgroup
        } else {
            FreezeMethod::Signal
        };

        if let Err(e) = self
            .journal
            .arm(app_id.key(), method, cgroup_path.clone(), procs)
        {
            if self
                .rate_limiter
                .allow(LogKey::app(app_id.as_str(), "journal-arm"))
            {
                warn!(
                    app_id = %app_id,
                    error = %e,
                    "cannot record freeze in journal, refusing to suspend: \
                     suspended processes that are not recorded cannot be recovered"
                );
            }
            return false;
        }

        let report = freeze::freeze_app(&self.cgroup_mgr, cgroup_path.as_deref(), procs);
        self.absorb_gone(app_id, &report);

        if let Err(e) = self.journal.commit(app_id.key(), procs, &report.applied) {
            debug!(app_id = %app_id, error = %e, "journal commit failed");
        }

        if report.nothing_applied() {
            self.on_suspend_failed(app_id, &report, "freeze");
            return false;
        }

        self.suspend_failures.remove(app_id);
        METRICS.apps_frozen_total.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Drop processes the operation found to have exited.
    ///
    /// This is the point-of-action half of dead-PID handling: the periodic sweep
    /// catches the rest, but reacting here stops a retry loop from re-signalling
    /// the same corpse seconds later.
    fn absorb_gone(&mut self, app_id: &AppId, report: &crate::system::apply::ApplyReport) {
        if report.gone.is_empty() {
            return;
        }
        debug!(app_id = %app_id, pids = ?report.gone, "processes already gone");
        METRICS
            .pids_reaped_total
            .fetch_add(report.gone.len() as u64, Ordering::Relaxed);
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.roots_mut().remove_pids(&report.gone);
        }
    }

    /// Report an operation that reached only part of its targets.
    ///
    /// The single place that decides *when* such a failure is worth a log line:
    /// the check for a real error and the per-application suppression window
    /// belong together, and having them together means the policy is changed in
    /// one place rather than at every call site that applies a report.
    pub(crate) fn warn_partial_failure(
        &mut self,
        app_id: &AppId,
        kind: &'static str,
        report: &crate::system::apply::ApplyReport,
    ) {
        if !report.had_real_error() {
            return;
        }
        if self.rate_limiter.allow(LogKey::app(app_id.as_str(), kind)) {
            warn!(
                app_id = %app_id,
                kind,
                failed = report.failed.len(),
                "operation partially failed"
            );
        }
    }

    /// Handle an attempt that reached nothing.
    ///
    /// Retries back off and eventually stop. The original ten-second retry never
    /// gave up, so a permanently unreachable target produced one log line per
    /// attempt for as long as the daemon ran.
    fn on_suspend_failed(
        &mut self,
        app_id: &AppId,
        report: &crate::system::apply::ApplyReport,
        what: &'static str,
    ) {
        if !report.had_real_error() {
            // Everything was already gone; the sweep will retire the app.
            debug!(app_id = %app_id, what, "nothing left to suspend");
            return;
        }

        let attempts = self.suspend_failures.entry(app_id.clone()).or_insert(0);
        *attempts += 1;
        let attempts = *attempts;

        // Keyed on the offending PID as well as the application, so a *different*
        // process failing is still reported rather than being masked by the
        // suppression window of an earlier, unrelated failure.
        if let Some((pid, error)) = report.failed.first() {
            if self
                .rate_limiter
                .allow(LogKey::pid(app_id.as_str(), *pid, "suspend-failed"))
            {
                warn!(
                    app_id = %app_id,
                    what,
                    attempts,
                    pid,
                    error = %error,
                    "suspend failed"
                );
            }
        }

        if attempts >= MAX_SUSPEND_ATTEMPTS {
            warn!(
                app_id = %app_id,
                what,
                attempts,
                "giving up on suspending this app until it is focused again"
            );
            if let Some(entry) = self.registry.get_mut(app_id) {
                entry.cancel_suspend_timer();
            }
            return;
        }

        // Exponential backoff, capped. With the current MAX_SUSPEND_ATTEMPTS the
        // exponent never leaves 0..=3, so the cap only binds if that limit is
        // raised; it is kept so raising it cannot produce an unbounded delay.
        let delay = RETRY_INTERVAL
            .saturating_mul(1 << (attempts - 1))
            .min(MAX_RETRY_INTERVAL);
        self.schedule_suspend(app_id, delay);
    }

    /// Schedule a retry of the suspend timer after RETRY_INTERVAL.
    pub(crate) fn reschedule_suspend(&mut self, app_id: &AppId) {
        self.schedule_suspend(app_id, RETRY_INTERVAL);
    }

    /// Restore a suspended app to Background state: thaw/unthrottle, then set Background.
    pub(crate) fn restore_to_background(&mut self, app_id: &AppId, state: AppState) {
        let procs = self.recorded_procs_for(app_id);
        let (new_state, action) = state.on_focus_gained();
        self.execute_transition(app_id, new_state, action, &procs);
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_state(AppState::Background);
            entry.cancel_all_timers();
        }
    }

    /// Schedule a suspend timer with a specific delay.
    pub(crate) fn schedule_suspend(&mut self, app_id: &AppId, delay: std::time::Duration) {
        let handle = Self::spawn_delayed_event(
            &self.event_tx,
            delay,
            EngineEvent::SuspendTimerFired {
                app_id: app_id.clone(),
            },
        );
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_suspend_timer(handle);
        }
    }

    /// The processes to act on when thawing without a freshly expanded tree.
    ///
    /// Prefers the journal, which holds everything that was actually signalled —
    /// including descendants that the registry never knew about and that would
    /// otherwise stay stopped.
    ///
    /// The result is what was *recorded*, not what is still alive: liveness is
    /// not checked here, so a caller that needs it must filter itself.
    pub(crate) fn recorded_procs_for(&self, app_id: &AppId) -> Vec<ProcessHandle> {
        let recorded: Vec<ProcessHandle> = self
            .journal
            .entries()
            .filter(|entry| entry.app_id == app_id.key())
            .flat_map(|entry| entry.procs.iter().copied())
            .collect();

        if !recorded.is_empty() {
            return recorded;
        }
        self.registry
            .get(app_id)
            .map(|entry| entry.roots().handles().to_vec())
            .unwrap_or_default()
    }

    /// Restore an app from Frozen or Throttled state to normal.
    ///
    /// Thaw is treated as at-least-once: a redundant `SIGCONT` is harmless,
    /// whereas a missed one leaves a process stopped with nobody left to
    /// release it. Only processes actually released — or already dead — are
    /// retired from the journal, so a partial thaw keeps the remainder
    /// recorded.
    pub(crate) fn restore_app_resources(
        &mut self,
        app_id: &AppId,
        state: AppState,
        cgroup_path: Option<&std::path::Path>,
        procs: &[ProcessHandle],
        elapsed_ms: u64,
    ) {
        match state {
            AppState::Frozen => {
                let report = freeze::thaw_app(&self.cgroup_mgr, cgroup_path, procs);
                self.warn_partial_failure(app_id, "thaw-failed", &report);
                if let Err(e) = self.journal.retire(app_id.key(), &report.settled_pids()) {
                    debug!(app_id = %app_id, error = %e, "journal retire failed");
                }
                self.absorb_gone(app_id, &report);
                METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .time_in_frozen_ms
                    .fetch_add(elapsed_ms, Ordering::Relaxed);
            }
            AppState::Throttled => {
                let report = throttle::remove_throttle(&self.cgroup_mgr, cgroup_path, procs);
                self.absorb_gone(app_id, &report);
                METRICS
                    .apps_unthrottled_total
                    .fetch_add(1, Ordering::Relaxed);
                METRICS
                    .time_in_throttled_ms
                    .fetch_add(elapsed_ms, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    /// Spawn a delayed event: sleep then send the event to the engine channel.
    pub(crate) fn spawn_delayed_event(
        tx: &mpsc::Sender<EngineEvent>,
        delay: std::time::Duration,
        event: EngineEvent,
    ) -> tokio::task::JoinHandle<()> {
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if tx.send(event).await.is_err() {
                debug!("engine channel closed, timer event dropped");
            }
        })
    }

    /// Record how long an app has been in its current state into the given metric.
    pub(crate) fn record_state_duration(&self, app_id: &AppId, metric: &AtomicU64) {
        if let Some(entry) = self.registry.get(app_id) {
            let elapsed_ms = entry.state_since().elapsed().as_millis() as u64;
            metric.fetch_add(elapsed_ms, Ordering::Relaxed);
        }
    }
}
