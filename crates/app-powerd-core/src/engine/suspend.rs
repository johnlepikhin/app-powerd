use super::*;

impl Engine {
    #[instrument(skip(self))]
    pub(crate) async fn handle_suspend_timer(&mut self, app_id: AppId) {
        let Some(entry) = self.registry.get(&app_id) else {
            return;
        };

        // Check resume grace
        if entry.in_resume_grace() {
            debug!(app_id = %app_id, "in resume grace, skipping suspend");
            return;
        }

        // Check min_suspend: app must have been in background long enough
        let min_suspend = entry.policy().min_suspend;
        let elapsed = entry.state_since().elapsed();
        if elapsed < min_suspend {
            let remaining = min_suspend - elapsed;
            debug!(app_id = %app_id, remaining_ms = remaining.as_millis(), "min_suspend not reached, rescheduling");
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                remaining,
                EngineEvent::SuspendTimerFired {
                    app_id: app_id.clone(),
                },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.set_suspend_timer(handle);
            }
            return;
        }

        let should_freeze = entry.policy().action == Action::Freeze;

        // Expand the process tree once, off the event loop, and reuse the result
        // for the guard checks and for the transition itself. It used to be
        // walked twice — once here and again inside `freeze_app`, that time
        // synchronously on this single-threaded runtime.
        //
        // The protection split runs in the same blocking task: it reads
        // `/proc/<pid>/comm` for every descendant, which for a browser tree is
        // hundreds of synchronous reads and would undo the point of moving the
        // walk off the loop. The policy cannot be borrowed into a `'static`
        // closure, so a copy of it goes along.
        let base_pids = entry.pids();
        let policy = self.protection.clone();
        let (procs, protected) = match tokio::time::timeout(
            DESCENDANT_PIDS_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let expanded = freeze::expand_tree(&base_pids);
                policy.partition(&expanded)
            }),
        )
        .await
        {
            Ok(Ok(split)) => split,
            Ok(Err(e)) => {
                warn!(app_id = %app_id, error = %e, "spawn_blocking for process tree failed");
                (Vec::new(), Vec::new())
            }
            Err(_) => {
                warn!(app_id = %app_id, "process tree scan timed out");
                (Vec::new(), Vec::new())
            }
        };

        // Session infrastructure that happens to sit below this app in the
        // process tree was excluded above — a browser or file manager routinely
        // has a portal helper or gvfsd among its descendants, and stopping those
        // blocks unrelated applications on D-Bus timeouts.
        if !protected.is_empty() {
            METRICS
                .protection_blocks_total
                .fetch_add(protected.len() as u64, Ordering::Relaxed);
            if self
                .rate_limiter
                .allow(LogKey::app(app_id.as_str(), "protected-descendants"))
            {
                info!(
                    app_id = %app_id,
                    count = protected.len(),
                    first = %protected[0].1,
                    "excluding protected processes from suspend"
                );
            }
        }
        let pids: Vec<u32> = procs.iter().map(|h| h.pid).collect();

        let Some(entry) = self.registry.get(&app_id) else {
            return;
        };
        let guards_config = entry.policy().guards.clone();
        let is_fullscreen = entry.window_info().is_fullscreen;

        // Check guards before suspending (async)
        let guard_result = guards::check_guards(&pids, &guards_config, is_fullscreen).await;
        if guard_result != GuardResult::Allow {
            if let GuardResult::Block(reason) = guard_result {
                info!(app_id = %app_id, reason = %reason, "guard blocked suspend");
                METRICS.guard_blocks_total.fetch_add(1, Ordering::Relaxed);
            }
            // Reschedule to recheck guards later
            self.reschedule_suspend(&app_id);
            return;
        }

        // Re-verify state after async guards check — user may have switched back
        let Some(entry) = self.registry.get(&app_id) else {
            return;
        };
        if entry.state() != AppState::Background {
            debug!(app_id = %app_id, state = %entry.state(), "state changed during guards check, skipping suspend");
            return;
        }

        let suspend_mode = if should_freeze {
            SuspendMode::Freeze
        } else {
            SuspendMode::Throttle
        };
        let (new_state, action) = entry.state().on_suspend_timer(suspend_mode);
        if action != TransitionAction::NoOp {
            info!(app_id = %app_id, to = %new_state, "suspend timer fired");
            self.execute_transition(&app_id, new_state, action, &procs);
        }
    }

    pub(crate) fn handle_maintenance_wake(&mut self, app_id: AppId) {
        let (cgroup_path, duration) = {
            let Some(entry) = self.registry.get(&app_id) else {
                return;
            };
            if entry.state() != AppState::Frozen {
                return;
            }
            (
                entry.cgroup_path_buf(),
                entry.policy().maintenance_resume.duration,
            )
        };

        info!(app_id = %app_id, "maintenance wake");
        // Release exactly what was recorded as frozen. The journal entry is kept
        // intact: this is a temporary wake, the app re-freezes in `duration`, and
        // a crash in between must still leave the processes recoverable.
        let procs = self.recorded_procs_for(&app_id);
        let report = freeze::thaw_app(&self.cgroup_mgr, cgroup_path.as_deref(), &procs);
        self.warn_partial_failure(&app_id, "maintenance-thaw", &report);
        self.record_state_duration(&app_id, &METRICS.time_in_frozen_ms);
        // Reset state_since to prevent double-counting on next maintenance wake
        if let Some(entry) = self.registry.get_mut(&app_id) {
            entry.reset_state_since();
        }
        METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);

        // Schedule re-freeze after duration
        let handle = Self::spawn_delayed_event(
            &self.event_tx,
            duration,
            EngineEvent::MaintenanceSleep {
                app_id: app_id.clone(),
            },
        );
        if let Some(entry) = self.registry.get_mut(&app_id) {
            entry.set_maintenance_timer(handle);
        }
    }

    pub(crate) fn handle_maintenance_sleep(&mut self, app_id: AppId) {
        let cgroup_path = {
            let Some(entry) = self.registry.get(&app_id) else {
                return;
            };
            if entry.state() != AppState::Frozen {
                return;
            }
            entry.cgroup_path_buf()
        };

        info!(app_id = %app_id, "maintenance sleep");
        // Re-freezing the same recorded set needs no journal write: the entry
        // already describes these processes as suspended, which is what recovery
        // acts on. At a 30-second maintenance interval, rewriting the file twice
        // a minute per app would be pure churn.
        let procs = self.recorded_procs_for(&app_id);
        let report = freeze::freeze_app(&self.cgroup_mgr, cgroup_path.as_deref(), &procs);
        self.warn_partial_failure(&app_id, "maintenance-freeze", &report);
        METRICS.apps_frozen_total.fetch_add(1, Ordering::Relaxed);

        // Schedule next wake
        self.start_maintenance_timer(&app_id);
    }

    pub(crate) fn start_maintenance_timer(&mut self, app_id: &AppId) {
        let Some(entry) = self.registry.get_mut(app_id) else {
            return;
        };

        let interval = entry.policy().maintenance_resume.interval;
        let handle = Self::spawn_delayed_event(
            &self.event_tx,
            interval,
            EngineEvent::MaintenanceWake {
                app_id: app_id.clone(),
            },
        );
        entry.set_maintenance_timer(handle);
    }

    pub(crate) fn start_management(&mut self) {
        info!("management activated, starting suspend timers for background apps");
        let background_apps: Vec<(AppId, std::time::Duration)> = self
            .registry
            .iter()
            .filter(|(_, e)| {
                e.state() == AppState::Background && e.policy().action != Action::Ignore
            })
            .map(|(id, e)| (id.clone(), e.policy().suspend_delay))
            .collect();

        for (app_id, delay) in background_apps {
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                delay,
                EngineEvent::SuspendTimerFired {
                    app_id: app_id.clone(),
                },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.set_suspend_timer(handle);
            }
        }
    }
}
