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

        // Clone data before await to release borrow on self.registry.
        // Note: PIDs may become stale during async guards check (process exits, PID reuse).
        // This is acceptable: guards may miss a check, but execute_transition re-reads fresh PIDs.
        let base_pids = entry.pids().to_vec();
        let mut pids = base_pids.clone();
        match tokio::time::timeout(
            DESCENDANT_PIDS_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                let mut descendants = Vec::new();
                for &pid in &base_pids {
                    descendants.extend(crate::system::process::descendant_pids(pid));
                }
                descendants
            }),
        )
        .await
        {
            Ok(Ok(descendants)) => pids.extend(descendants),
            Ok(Err(e)) => {
                warn!(app_id = %app_id, error = %e, "spawn_blocking for descendant_pids failed")
            }
            Err(_) => warn!(app_id = %app_id, "descendant_pids timed out after 5s"),
        }
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
            let handle = Self::spawn_delayed_event(
                &self.event_tx,
                RETRY_INTERVAL,
                EngineEvent::SuspendTimerFired {
                    app_id: app_id.clone(),
                },
            );
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.set_suspend_timer(handle);
            }
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
            self.execute_transition(&app_id, new_state, action);
        }
    }

    pub(crate) fn handle_maintenance_wake(&mut self, app_id: AppId) {
        // Clone data needed after mutable borrow
        let (cgroup_path, pids, duration) = {
            let Some(entry) = self.registry.get(&app_id) else {
                return;
            };
            if entry.state() != AppState::Frozen {
                return;
            }
            (
                entry.cgroup_path_buf(),
                entry.pids().to_vec(),
                entry.policy().maintenance_resume.duration,
            )
        };

        info!(app_id = %app_id, "maintenance wake");
        if let Err(e) = freeze::thaw_app(&self.cgroup_mgr, cgroup_path.as_deref(), &pids) {
            warn!(app_id = %app_id, error = %e, "maintenance thaw failed");
        }
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
        let Some(entry) = self.registry.get(&app_id) else {
            return;
        };
        if entry.state() != AppState::Frozen {
            return;
        }

        info!(app_id = %app_id, "maintenance sleep");
        if let Err(e) = freeze::freeze_app(&self.cgroup_mgr, entry.cgroup_path_ref(), entry.pids())
        {
            warn!(app_id = %app_id, error = %e, "maintenance freeze failed");
        }
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
