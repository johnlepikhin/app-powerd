use super::*;

impl Engine {
    pub(crate) fn handle_config_reload(&mut self, config: Config) {
        match RulesEngine::new(config.clone()) {
            Ok(engine) => {
                let was_managing = self.should_manage();
                self.rules_engine = engine;
                self.enabled = config.defaults.enabled;
                METRICS.config_reloads_total.fetch_add(1, Ordering::Relaxed);
                info!("config reloaded successfully");

                self.rematch_tracked_apps();

                if !self.should_manage() {
                    self.thaw_all();
                } else if !was_managing {
                    self.start_management();
                }
            }
            Err(e) => {
                error!(error = %e, "config reload failed");
            }
        }
    }

    /// Re-match all tracked apps against the new rules and update policies.
    /// If the effective action changes, restore/reschedule as needed.
    fn rematch_tracked_apps(&mut self) {
        // Collect match contexts from all tracked apps (immutable borrow)
        let app_contexts: Vec<_> = self
            .registry
            .iter()
            .map(|(id, entry)| {
                let mut ctx = MatchContext::from(entry.window_info());
                if let Some(exe) = &entry.window_info().executable {
                    if let Some(desktop_id) = self.exe_to_desktop.get(exe.as_str()) {
                        ctx.desktop_file = desktop_id.clone();
                    }
                }
                (id.clone(), ctx, entry.state(), entry.policy().action)
            })
            .collect();

        for (app_id, ctx, state, old_action) in app_contexts {
            let new_policy = self.rules_engine.match_window(&ctx);
            let new_action = new_policy.action;

            // Update the policy
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.set_policy(new_policy.clone());
            }

            // Handle action changes for suspended apps
            match (state, old_action, new_action) {
                // Frozen/Throttled + new Ignore → restore and cancel timers
                (AppState::Frozen | AppState::Throttled, _, Action::Ignore) => {
                    self.restore_to_background(&app_id, state);
                    info!(app_id = %app_id, "policy changed to ignore, restored from {state}");
                }

                // Frozen + new Throttle → restore, reschedule as throttle
                (AppState::Frozen, _, Action::Throttle) => {
                    self.restore_to_background(&app_id, state);
                    self.schedule_suspend(&app_id, new_policy.suspend_delay);
                    info!(app_id = %app_id, "policy changed from freeze to throttle, rescheduling");
                }

                // Throttled + new Freeze → restore, reschedule as freeze
                (AppState::Throttled, _, Action::Freeze) => {
                    self.restore_to_background(&app_id, state);
                    self.schedule_suspend(&app_id, new_policy.suspend_delay);
                    info!(app_id = %app_id, "policy changed from throttle to freeze, rescheduling");
                }

                // Background + new Ignore → cancel suspend timer
                (AppState::Background, _, Action::Ignore) => {
                    if let Some(entry) = self.registry.get_mut(&app_id) {
                        entry.cancel_suspend_timer();
                    }
                    info!(app_id = %app_id, "policy changed to ignore, cancelled suspend timer");
                }

                // Background + changed timing → cancel and reschedule
                (AppState::Background, _, _)
                    if new_action != Action::Ignore && old_action != Action::Ignore =>
                {
                    if let Some(entry) = self.registry.get_mut(&app_id) {
                        entry.cancel_suspend_timer();
                    }
                    self.schedule_suspend(&app_id, new_policy.suspend_delay);
                }

                // Everything else — no-op
                _ => {}
            }
        }
    }

    pub(crate) fn handle_power_change(&mut self, source: PowerSource) {
        let was_managing = self.should_manage();
        info!(?source, "power source changed");
        self.power_source = source;

        if !self.should_manage() {
            self.thaw_all();
        } else if !was_managing {
            self.start_management();
        }
    }

    pub(crate) fn should_manage(&self) -> bool {
        if !self.enabled {
            return false;
        }

        let config = self.rules_engine.config();
        match self.power_source {
            PowerSource::Ac => config.defaults.mode.ac == PowerMode::Enable,
            PowerSource::Battery => config.defaults.mode.battery == PowerMode::Enable,
            PowerSource::Unknown => true,
        }
    }

    pub(crate) fn thaw_all(&mut self) {
        let transitions: Vec<_> = self
            .registry
            .iter()
            .filter(|(_, e)| matches!(e.state(), AppState::Frozen | AppState::Throttled))
            .map(|(id, e)| {
                let (new_state, action) = e.state().on_focus_gained();
                (id.clone(), new_state, action)
            })
            .collect();

        for (app_id, new_state, action) in transitions {
            self.execute_transition(&app_id, new_state, action);
        }
    }

    pub(crate) fn shutdown(&mut self) {
        info!("graceful shutdown: thawing all apps");
        self.thaw_all();

        // Cancel all timers and clean up cgroups
        for (_, entry) in self.registry.iter_mut() {
            entry.cancel_all_timers();
            if let Some(path) = entry.cgroup_path_ref() {
                if let Err(e) = self.cgroup_mgr.remove_cgroup(path) {
                    debug!(error = %e, "failed to remove cgroup during shutdown");
                }
            }
        }
    }
}
