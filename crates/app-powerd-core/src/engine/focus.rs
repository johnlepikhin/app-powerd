use super::*;

impl Engine {
    #[instrument(skip(self))]
    pub(crate) fn handle_focus_changed(&mut self, window: WindowInfo) {
        METRICS.focus_changes_total.fetch_add(1, Ordering::Relaxed);

        let app_id = AppId::from_window(&window);
        debug!(app_id = %app_id, window_id = window.window_id, "focus changed");

        if self.registry.get(&app_id).is_some() {
            self.activate_existing_app(&app_id, window);
        } else {
            self.register_new_app(app_id.clone(), window);
        }

        self.background_other_active_apps(&app_id);
    }

    fn activate_existing_app(&mut self, app_id: &AppId, window: WindowInfo) {
        let (new_state, action, old_state) = {
            let Some(entry) = self.registry.get(app_id) else {
                return;
            };
            let (new_state, action) = entry.state().on_focus_gained();
            (new_state, action, entry.state())
        };

        if action != TransitionAction::NoOp {
            info!(app_id = %app_id, from = %old_state, to = %new_state, "activating");
            self.execute_transition(app_id, new_state, action);
        }

        // Move new PID to existing cgroup if needed
        if let Some(pid) = window.pid {
            let needs_move = self
                .registry
                .get(app_id)
                .map(|e| !e.contains_pid(pid))
                .unwrap_or(false);
            if needs_move {
                let cgroup = self.registry.get(app_id).and_then(|e| e.cgroup_path_buf());
                if let Some(ref path) = cgroup {
                    if let Err(e) = self.cgroup_mgr.move_pid(path, pid) {
                        warn!(pid, error = %e, "failed to move new pid to cgroup");
                    }
                }
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.add_pid(pid);
                }
            }
        }

        if let Some(entry) = self.registry.get_mut(app_id) {
            let wid = window.window_id;
            entry.update_window_info(window);
            entry.add_window(wid);
        }
    }

    fn register_new_app(&mut self, app_id: AppId, window: WindowInfo) {
        let mut ctx = MatchContext::from(&window);
        if let Some(exe) = &window.executable {
            if let Some(desktop_id) = self.exe_to_desktop.get(exe.as_str()) {
                ctx.desktop_file = desktop_id.clone();
            }
        }
        let policy = self.rules_engine.match_window(&ctx);
        info!(app_id = %app_id, action = ?policy.action, "new app tracked");

        let entry = AppEntry::new(app_id.clone(), window, policy);
        self.registry.insert(entry);

        self.setup_cgroup(&app_id);
    }

    fn background_other_active_apps(&mut self, focused_app: &AppId) {
        let transitions: Vec<_> = self
            .registry
            .iter()
            .filter(|(id, e)| **id != *focused_app && e.state() == AppState::Active)
            .map(|(id, e)| {
                let (new_state, action) = e.state().on_focus_lost();
                (id.clone(), new_state, action, e.state())
            })
            .collect();

        for (other_id, new_state, action, old_state) in transitions {
            if action != TransitionAction::NoOp {
                info!(app_id = %other_id, from = %old_state, to = %new_state, "backgrounding");
                self.execute_transition(&other_id, new_state, action);
            }
        }
    }

    pub(crate) fn handle_window_closed(&mut self, window_id: u64) {
        if let Some(entry) = self.registry.remove_window(window_id) {
            info!(app_id = %entry.app_id(), "app removed (all windows closed)");

            let elapsed_ms = entry.state_since().elapsed().as_millis() as u64;

            // Restore app to normal state before removing
            self.restore_app_resources(
                entry.app_id(),
                entry.state(),
                entry.cgroup_path_ref(),
                entry.pids(),
                elapsed_ms,
            );

            // Clean up cgroup
            if let Some(path) = entry.cgroup_path_ref() {
                if let Err(e) = self.cgroup_mgr.remove_cgroup(path) {
                    warn!(app_id = %entry.app_id(), error = %e, "failed to remove cgroup on window close");
                }
            }
        }
    }

    pub(crate) fn setup_cgroup(&mut self, app_id: &AppId) {
        use crate::system::cgroup::CgroupCapability;

        let pids = {
            let Some(entry) = self.registry.get(app_id) else {
                return;
            };
            if entry.policy().action == Action::Ignore {
                return;
            }
            entry.pids().to_vec()
        };

        match self.cgroup_mgr.create_cgroup(app_id.as_str(), &pids) {
            Ok(path) => {
                // DirectWrite: PIDs must be moved explicitly (systemd does it via D-Bus)
                if self.cgroup_mgr.capability() == CgroupCapability::DirectWrite {
                    for &pid in &pids {
                        if let Err(e) = self.cgroup_mgr.move_pid(&path, pid) {
                            warn!(pid, error = %e, "failed to move pid to cgroup");
                        }
                    }
                }
                if let Some(entry) = self.registry.get_mut(app_id) {
                    entry.set_cgroup_path(path);
                }
            }
            Err(e) => {
                info!(app_id = %app_id, error = %e, "cgroup setup failed, will use signal fallback");
            }
        }
    }
}
