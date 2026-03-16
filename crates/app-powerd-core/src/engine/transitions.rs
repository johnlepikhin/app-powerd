use super::*;

impl Engine {
    #[instrument(skip(self))]
    pub(crate) fn execute_transition(
        &mut self,
        app_id: &AppId,
        new_state: AppState,
        action: TransitionAction,
    ) {
        // Actions that require active management: skip if management disabled
        if !self.should_manage() && action.requires_management() {
            if let Some(entry) = self.registry.get_mut(app_id) {
                entry.set_state(new_state);
            }
            return;
        }

        // Clone what we need before mutating
        let (pids, cgroup_path, policy) = {
            let Some(entry) = self.registry.get(app_id) else {
                return;
            };
            (
                entry.pids().to_vec(),
                entry.cgroup_path_buf(),
                entry.policy().clone(),
            )
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
                if let Err(e) = throttle::apply_throttle(
                    &self.cgroup_mgr,
                    cgroup_p,
                    &pids,
                    &policy.throttle_params(),
                ) {
                    warn!(app_id = %app_id, error = %e, "throttle failed, rescheduling");
                    self.reschedule_suspend(app_id);
                    return;
                }
                METRICS.apps_throttled_total.fetch_add(1, Ordering::Relaxed);
            }
            TransitionAction::ApplyFreeze => {
                if let Err(e) = freeze::freeze_app(&self.cgroup_mgr, cgroup_p, &pids) {
                    warn!(app_id = %app_id, error = %e, "freeze failed, rescheduling");
                    self.reschedule_suspend(app_id);
                    return;
                }
                METRICS.apps_frozen_total.fetch_add(1, Ordering::Relaxed);

                // Start maintenance timer if enabled
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
                if !self.restore_app_resources(app_id, current_state, cgroup_p, &pids, elapsed_ms) {
                    return;
                }
            }
            TransitionAction::NoOp => {}
        }

        // Update state
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_state(new_state);
        }
    }

    /// Schedule a retry of the suspend timer after RETRY_INTERVAL.
    pub(crate) fn reschedule_suspend(&mut self, app_id: &AppId) {
        let handle = Self::spawn_delayed_event(
            &self.event_tx,
            RETRY_INTERVAL,
            EngineEvent::SuspendTimerFired {
                app_id: app_id.clone(),
            },
        );
        if let Some(entry) = self.registry.get_mut(app_id) {
            entry.set_suspend_timer(handle);
        }
    }

    /// Restore a suspended app to Background state: thaw/unthrottle, then set Background.
    pub(crate) fn restore_to_background(&mut self, app_id: &AppId, state: AppState) {
        let (new_state, action) = state.on_focus_gained();
        self.execute_transition(app_id, new_state, action);
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

    /// Restore an app from Frozen or Throttled state to normal.
    /// Returns `true` on success, `false` on error.
    pub(crate) fn restore_app_resources(
        &self,
        app_id: &AppId,
        state: AppState,
        cgroup_path: Option<&std::path::Path>,
        pids: &[u32],
        elapsed_ms: u64,
    ) -> bool {
        match state {
            AppState::Frozen => {
                if let Err(e) = freeze::thaw_app(&self.cgroup_mgr, cgroup_path, pids) {
                    warn!(app_id = %app_id, error = %e, "thaw failed");
                    return false;
                }
                METRICS.apps_thawed_total.fetch_add(1, Ordering::Relaxed);
                METRICS
                    .time_in_frozen_ms
                    .fetch_add(elapsed_ms, Ordering::Relaxed);
            }
            AppState::Throttled => {
                if let Err(e) = throttle::remove_throttle(&self.cgroup_mgr, cgroup_path, pids) {
                    warn!(app_id = %app_id, error = %e, "remove throttle failed");
                    return false;
                }
                METRICS
                    .apps_unthrottled_total
                    .fetch_add(1, Ordering::Relaxed);
                METRICS
                    .time_in_throttled_ms
                    .fetch_add(elapsed_ms, Ordering::Relaxed);
            }
            _ => {}
        }
        true
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
