use super::*;

impl Engine {
    pub(crate) fn handle_ipc(&mut self, request: IpcRequest) -> IpcResponse {
        match request {
            IpcRequest::List => {
                let apps = self
                    .registry
                    .iter()
                    .map(|(id, entry)| {
                        let policy = entry.policy();
                        AppInfo {
                            app_id: id.to_string(),
                            state: entry.state(),
                            pids: entry.pids(),
                            executable: entry.window_info().executable.clone(),
                            wm_class: entry.window_info().wm_class.clone(),
                            window_title: entry.window_info().title.clone(),
                            profile: policy.matched_profile.clone(),
                            rule_id: policy.matched_rule.clone(),
                            state_since_secs: entry.state_since().elapsed().as_secs(),
                            protected: self
                                .protection_verdict(entry.window_info())
                                .map(|reason| reason.to_string()),
                        }
                    })
                    .collect();
                IpcResponse::AppList { apps }
            }
            IpcRequest::Status => {
                let caps = self.cgroup_mgr.capabilities();
                let protected_apps = self
                    .registry
                    .iter()
                    .filter(|(_, entry)| self.protection_verdict(entry.window_info()).is_some())
                    .count();
                IpcResponse::Status {
                    enabled: self.enabled && self.should_manage(),
                    power_source: self.power_source,
                    forced_power_source: self.forced_power_source,
                    tracked_apps: self.registry.len(),
                    uptime_secs: self.start_time.elapsed().as_secs(),
                    cgroup_mode: caps.tier.to_string(),
                    cpu_control: caps.cpu_control,
                    protected_apps,
                    protocol_version: crate::ipc::protocol::PROTOCOL_VERSION,
                }
            }
            IpcRequest::SetPowerOverride { source } => {
                if matches!(source, Some(PowerSource::Unknown)) {
                    return IpcResponse::Error {
                        message: "cannot force power source to 'unknown'".into(),
                    };
                }
                self.handle_set_power_override(source);
                // User-facing label; CLI/scripts may parse this — keep stable.
                let label = source.map_or("auto".to_string(), |s| s.to_string());
                IpcResponse::Ok {
                    message: format!("power source override set to {label}"),
                }
            }
            IpcRequest::Stats => IpcResponse::Stats {
                metrics: METRICS.snapshot(),
            },
            IpcRequest::Freeze { target } => self.ipc_freeze(target),
            IpcRequest::Thaw { target } => self.ipc_thaw(target),
            IpcRequest::ThawAll => self.ipc_thaw_all(),
            IpcRequest::ReloadConfig => match load_config(&self.config_path) {
                Ok(new_config) => {
                    let tx = self.event_tx.clone();
                    match tx.try_send(EngineEvent::ConfigReloaded(new_config)) {
                        Ok(()) => IpcResponse::Ok {
                            message: "config reload triggered".into(),
                        },
                        Err(e) => IpcResponse::Error {
                            message: format!("failed to queue config reload: {e}"),
                        },
                    }
                }
                Err(e) => IpcResponse::Error {
                    message: format!("config reload failed: {e}"),
                },
            },
            IpcRequest::Shutdown => {
                if let Err(e) = self.event_tx.try_send(EngineEvent::Shutdown) {
                    error!("Failed to send shutdown event: {}", e);
                }
                IpcResponse::Ok {
                    message: "shutdown scheduled".into(),
                }
            }
        }
    }

    fn ipc_freeze(&mut self, target: Target) -> IpcResponse {
        let procs = match self.resolve_target(&target) {
            Ok(procs) => procs,
            Err(message) => return IpcResponse::Error { message },
        };

        // The deny-list is not overridable by a manual command either: a user
        // freezing xdg-desktop-portal by hand breaks their session just as
        // thoroughly as a rule doing it.
        // Everything resolved here was named by the user, directly or through an
        // application they asked for, so the session-bus tier does not apply —
        // only the built-in deny-list, which is not overridable by hand either.
        let own: Vec<u32> = procs.iter().map(|h| h.pid).collect();
        let (allowed, protected) = self.protection.partition(&procs, &own);
        if allowed.is_empty() {
            let reason = protected
                .first()
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| "no live processes".into());
            METRICS
                .protection_blocks_total
                .fetch_add(1, Ordering::Relaxed);
            return IpcResponse::Error {
                message: format!("refusing to freeze {target}: {reason}"),
            };
        }

        // Record before signalling, exactly as the automatic path does. A
        // manual freeze suspends real processes, so if it is not journalled the
        // daemon can be killed and nothing — neither startup recovery nor
        // `thaw-all` — will know those processes need releasing.
        let key = journal_key_for(&target);
        if let Err(e) = self.journal.arm(&key, FreezeMethod::Signal, None, &allowed) {
            return IpcResponse::Error {
                message: format!(
                    "refusing to freeze {target}: cannot record it in the freeze journal ({e}); \
                     suspended processes that are not recorded cannot be recovered"
                ),
            };
        }

        let report = freeze::freeze_app(&self.cgroup_mgr, None, &allowed);
        if let Err(e) = self.journal.commit(&key, &allowed, &report.applied) {
            debug!(error = %e, "journal commit failed after manual freeze");
        }

        if report.nothing_applied() {
            return IpcResponse::Error {
                message: format!("freeze failed for {target}: no process could be suspended"),
            };
        }
        IpcResponse::Ok {
            message: format!("froze {} process(es) for {target}", report.applied.len()),
        }
    }

    fn ipc_thaw(&mut self, target: Target) -> IpcResponse {
        let procs = match self.resolve_target(&target) {
            Ok(procs) => procs,
            Err(message) => return IpcResponse::Error { message },
        };
        let report = freeze::thaw_app(&self.cgroup_mgr, None, &procs);

        // Retire only what was actually released or already dead; anything that
        // failed stays recorded so shutdown and the next startup still know it
        // needs releasing.
        let settled = report.settled_pids();
        if let Err(e) = self.journal.retire(&journal_key_for(&target), &settled) {
            debug!(error = %e, "journal retire failed after manual thaw");
        }

        if let Target::App { app } = &target {
            let app_id = AppId::new(app.clone());
            // An application may also carry an entry from the automatic path
            // under its own key.
            if let Err(e) = self.journal.retire(app_id.key(), &settled) {
                debug!(error = %e, "journal retire failed after manual thaw");
            }
            if let Some(entry) = self.registry.get_mut(&app_id) {
                entry.set_state(AppState::Background);
            }
        }

        if report.nothing_applied() {
            return IpcResponse::Error {
                message: format!("thaw failed for {target}: no process could be resumed"),
            };
        }
        IpcResponse::Ok {
            message: format!("thawed {} process(es) for {target}", report.applied.len()),
        }
    }

    /// Release everything the daemon has suspended.
    ///
    /// Driven by the journal rather than the registry: the journal records what
    /// was actually signalled, including descendants and applications whose
    /// registry entry has since been dropped.
    fn ipc_thaw_all(&mut self) -> IpcResponse {
        let (released, stuck) = self.release_recorded_processes();

        for (_, entry) in self.registry.iter_mut() {
            if matches!(entry.state(), AppState::Frozen | AppState::Throttled) {
                entry.set_state(AppState::Background);
                entry.cancel_all_timers();
            }
        }

        if stuck > 0 {
            return IpcResponse::Error {
                message: format!(
                    "released {released} process(es), but {stuck} could not be resumed \
                     and remain recorded in the journal"
                ),
            };
        }
        IpcResponse::Ok {
            message: format!("released {released} process(es)"),
        }
    }

    /// Send `SIGCONT` to everything the journal records, retiring only what was
    /// released or already dead.
    ///
    /// Returns `(released, stuck)`. Clearing the journal wholesale would be
    /// wrong: a process we failed to resume is exactly the one that still needs
    /// the record.
    pub(crate) fn release_recorded_processes(&mut self) -> (usize, usize) {
        let mut released = 0usize;
        let mut stuck = 0usize;

        let entries: Vec<(String, Vec<ProcessHandle>)> = self
            .journal
            .entries()
            .map(|entry| (entry.app_id.clone(), entry.procs.clone()))
            .collect();

        for (key, procs) in entries {
            let mut settled = Vec::new();
            for handle in procs {
                if !handle.is_alive() {
                    settled.push(handle.pid);
                    continue;
                }
                match crate::system::freeze::signal_cont(handle.pid) {
                    Ok(()) => {
                        settled.push(handle.pid);
                        released += 1;
                    }
                    Err(e) => {
                        stuck += 1;
                        warn!(pid = handle.pid, error = %e, "could not resume recorded process");
                    }
                }
            }
            if let Err(e) = self.journal.retire(&key, &settled) {
                warn!(app_id = %key, error = %e, "journal retire failed");
            }
        }

        // Nothing is recorded any more, so the file itself can go. It is only
        // removed once every entry has been retired — never as a shortcut that
        // would also discard processes we failed to resume.
        if self.journal.is_empty() {
            if let Err(e) = self.journal.clear() {
                warn!(error = %e, "failed to remove the emptied freeze journal");
            }
        }

        (released, stuck)
    }

    /// Turn an IPC target into the set of processes to act on.
    fn resolve_target(&self, target: &Target) -> Result<Vec<ProcessHandle>, String> {
        match target {
            Target::Pid { pid } => {
                let handle = validate_ipc_pid(*pid)?;
                Ok(vec![handle])
            }
            Target::App { app } => {
                let app_id = AppId::new(app.clone());
                let procs = self.recorded_procs_for(&app_id);
                if procs.is_empty() {
                    return Err(format!("no tracked application named '{app}'"));
                }
                // Ownership is checked here too: the registry is populated from
                // window properties, which are not a claim of ownership.
                Ok(procs
                    .into_iter()
                    .filter(|handle| handle.is_alive() && handle.is_owned())
                    .collect())
            }
        }
    }
}

/// Journal key for a manually targeted freeze.
///
/// An application shares the key the automatic path uses, so a manual freeze
/// and an automatic one describe the same entry. A bare PID has no application
/// identity, so it gets a namespaced key of its own.
fn journal_key_for(target: &Target) -> String {
    match target {
        Target::Pid { pid } => format!("ipc:pid:{pid}"),
        Target::App { app } => AppId::new(app.clone()).key().to_string(),
    }
}

/// Validate a PID for IPC Freeze/Thaw commands.
///
/// Capturing the identity and checking ownership are separate obligations:
/// `ProcessHandle::open` removes the time-of-check/time-of-use gap of a bare
/// `/proc` stat, but says nothing about whether we are allowed to signal the
/// process it names.
fn validate_ipc_pid(pid: u32) -> Result<ProcessHandle, String> {
    if pid == 0 {
        return Err("pid 0 is not a valid target".into());
    }
    let handle = ProcessHandle::open(pid).ok_or_else(|| format!("no process with pid {pid}"))?;
    if !handle.is_owned() {
        return Err(format!("pid {pid} is not owned by current user"));
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ipc_pid_zero() {
        assert!(validate_ipc_pid(0).is_err());
    }

    #[test]
    fn validate_ipc_pid_self() {
        assert!(validate_ipc_pid(std::process::id()).is_ok());
    }

    #[test]
    fn validate_ipc_pid_nonexistent() {
        assert!(validate_ipc_pid(u32::MAX).is_err());
    }

    #[test]
    fn target_parse_distinguishes_pid_from_name() {
        assert!(matches!(Target::parse("1234"), Target::Pid { pid: 1234 }));
        assert!(matches!(Target::parse("Firefox"), Target::App { .. }));
    }
}
