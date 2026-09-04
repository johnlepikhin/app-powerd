use super::*;

impl Engine {
    /// `window` is skipped from the span on purpose: its `Debug` form carries
    /// the window title and command line, which would put document names, chat
    /// topics and URLs into the log on every focus change.
    #[instrument(skip(self, window), fields(window_id = window.window_id))]
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
            // Thawing acts on the journal's record, which includes descendants
            // the registry never saw.
            let procs = self.recorded_procs_for(app_id);
            self.execute_transition(app_id, new_state, action, &procs);
            // A user-driven activation clears the failure history: whatever was
            // wrong may well have been resolved by the app being interacted with.
            self.suspend_failures.remove(app_id);
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
                    if entry.add_pid(pid).is_none() {
                        debug!(pid, "window pid could not be identified, not tracking it");
                    }
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
        let mut policy = self.rules_engine.match_window(&ctx);

        // Protection is applied here, at policy resolution, rather than at the
        // moment of freezing. An application forced to `Ignore` never gets a
        // suspend timer at all, so "never frozen under any rule" holds by
        // construction instead of depending on a check further down the path.
        if let Some(reason) = self.protection_verdict(&window) {
            if policy.action != Action::Ignore {
                METRICS
                    .protection_blocks_total
                    .fetch_add(1, Ordering::Relaxed);
                if self
                    .rate_limiter
                    .allow(LogKey::app(app_id.as_str(), "protected"))
                {
                    warn!(
                        app_id = %app_id,
                        reason = %reason,
                        requested = ?policy.action,
                        "refusing to manage a protected process; \
                         the built-in deny-list overrides configuration"
                    );
                }
            }
            policy.action = Action::Ignore;
            policy.matched_rule = Some(format!("built-in protection: {reason}"));
        }

        info!(app_id = %app_id, action = ?policy.action, "new app tracked");

        let entry = AppEntry::new(app_id.clone(), window, policy);
        self.registry.insert(entry);

        self.setup_cgroup(&app_id);
    }

    /// Whether this window's process must never be suspended.
    pub(crate) fn protection_verdict(&self, window: &WindowInfo) -> Option<ProtectionReason> {
        let exe = window.executable.as_deref();
        match window.pid {
            Some(pid) => self.protection.check(pid, exe),
            // No PID at all: nothing can be signalled anyway, and treating an
            // identification gap as "safe to freeze" is how a deny-list gets
            // bypassed.
            None => exe
                .and_then(|e| self.protection.check(0, Some(e)))
                .or(Some(ProtectionReason::Unidentifiable)),
        }
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
                // Backgrounding only starts a timer; it touches no process.
                self.execute_transition(&other_id, new_state, action, &[]);
            }
        }
    }

    /// Pre-thaw any tracked frozen app whose window lives on the newly-active
    /// virtual desktop. Without this, switching to a workspace where a frozen
    /// app's window is unmapped (e.g., minimized to tray) leaves the user stuck
    /// — no `_NET_ACTIVE_WINDOW` change happens because the WM has no
    /// focusable window to activate.
    #[instrument(skip(self), fields(desktop))]
    pub(crate) fn handle_workspace_changed(&mut self, desktop: u32) {
        if !self.should_manage() {
            return;
        }

        let candidates: Vec<(AppId, std::time::Duration)> = self
            .registry
            .iter()
            .filter(|(_, e)| e.state() == AppState::Frozen)
            .filter(|(_, e)| e.window_info().desktop.is_some_and(|d| d.matches(desktop)))
            .map(|(id, e)| (id.clone(), e.policy().suspend_delay))
            .collect();

        for (app_id, delay) in candidates {
            self.pre_thaw_frozen(&app_id, delay, "workspace");
        }
    }

    /// Pre-thaw a tracked frozen app whose window was targeted by an
    /// `_NET_ACTIVE_WINDOW` ClientMessage from a panel or launcher. Without
    /// this, the WM would forward the activation to a still-SIGSTOP'd process
    /// that cannot answer.
    #[instrument(skip(self))]
    pub(crate) fn handle_activation_requested(&mut self, window_id: u64) {
        if !self.should_manage() {
            return;
        }
        let target = self
            .registry
            .iter()
            .find(|(_, e)| e.has_window(window_id) && e.state() == AppState::Frozen)
            .map(|(id, e)| (id.clone(), e.policy().suspend_delay));
        if let Some((app_id, delay)) = target {
            self.pre_thaw_frozen(&app_id, delay, "activation");
        }
    }

    /// Thaw a frozen app and park it in Background with a fresh suspend timer.
    /// We don't have a real focus event, so going straight to Active would
    /// leave multiple apps Active simultaneously when several pre-thaws fire
    /// for the same workspace. Background is the right resting state: a
    /// subsequent real focus event promotes it to Active, otherwise it
    /// re-freezes after `suspend_delay`.
    #[instrument(skip(self))]
    fn pre_thaw_frozen(
        &mut self,
        app_id: &AppId,
        suspend_delay: std::time::Duration,
        trigger: &'static str,
    ) {
        info!(app_id = %app_id, trigger, "pre-thaw");
        self.restore_to_background(app_id, AppState::Frozen);
        self.schedule_suspend(app_id, suspend_delay);
    }

    pub(crate) fn handle_window_closed(&mut self, window_id: u64) {
        let Some(entry) = self.registry.remove_window(window_id) else {
            return;
        };
        info!(app_id = %entry.app_id(), "app removed (all windows closed)");

        let app_id = entry.app_id().clone();
        let elapsed_ms = entry.state_since().elapsed().as_millis() as u64;
        let cgroup_path = entry.cgroup_path_buf();

        // Release the app before dropping it. The set comes from the journal so
        // descendants are covered: a window can close while helper processes it
        // spawned are still stopped, and once the entry is gone nothing else
        // would ever revisit them. Anything that fails to release stays in the
        // journal for the periodic sweep and for shutdown.
        let procs = self.recorded_procs_for(&app_id);
        self.restore_app_resources(
            &app_id,
            entry.state(),
            cgroup_path.as_deref(),
            &procs,
            elapsed_ms,
        );

        if let Some(path) = cgroup_path.as_deref() {
            if let Err(e) = self.cgroup_mgr.remove_cgroup(path) {
                debug!(app_id = %app_id, error = %e, "failed to remove cgroup on window close");
            }
        }

        self.suspend_failures.remove(&app_id);
        self.rate_limiter.forget_app(app_id.as_str());
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
            entry.pids()
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
