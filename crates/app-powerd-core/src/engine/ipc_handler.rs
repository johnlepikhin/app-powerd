use super::*;

impl Engine {
    pub(crate) fn handle_ipc(&mut self, request: IpcRequest) -> IpcResponse {
        match request {
            IpcRequest::List => {
                let apps = self
                    .registry
                    .iter()
                    .map(|(id, entry)| AppInfo {
                        app_id: id.to_string(),
                        state: entry.state(),
                        pids: entry.pids().to_vec(),
                        executable: entry.window_info().executable.clone(),
                        wm_class: entry.window_info().wm_class.clone(),
                        window_title: entry.window_info().title.clone(),
                    })
                    .collect();
                IpcResponse::AppList { apps }
            }
            IpcRequest::Status => IpcResponse::Status {
                enabled: self.enabled && self.should_manage(),
                power_source: self.power_source,
                forced_power_source: self.forced_power_source,
                tracked_apps: self.registry.len(),
                uptime_secs: self.start_time.elapsed().as_secs(),
            },
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
            IpcRequest::Freeze { pid } => {
                if let Err(msg) = validate_ipc_pid(pid) {
                    return IpcResponse::Error { message: msg };
                }
                match freeze::freeze_app(&self.cgroup_mgr, None, &[pid]) {
                    Ok(()) => IpcResponse::Ok {
                        message: format!("freeze signal sent to pid {pid}"),
                    },
                    Err(e) => IpcResponse::Error {
                        message: format!("freeze failed for pid {pid}: {e}"),
                    },
                }
            }
            IpcRequest::Thaw { pid } => {
                if let Err(msg) = validate_ipc_pid(pid) {
                    return IpcResponse::Error { message: msg };
                }
                match freeze::thaw_app(&self.cgroup_mgr, None, &[pid]) {
                    Ok(()) => IpcResponse::Ok {
                        message: format!("thaw signal sent to pid {pid}"),
                    },
                    Err(e) => IpcResponse::Error {
                        message: format!("thaw failed for pid {pid}: {e}"),
                    },
                }
            }
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
}

/// Validate a PID for IPC Freeze/Thaw commands.
fn validate_ipc_pid(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("pid 0 is not a valid target".into());
    }
    if !is_owned_pid(pid) {
        return Err(format!("pid {pid} is not owned by current user"));
    }
    Ok(())
}

/// Check if the given PID belongs to the current user.
fn is_owned_pid(pid: u32) -> bool {
    let metadata = match std::fs::metadata(format!("/proc/{pid}")) {
        Ok(m) => m,
        Err(_) => return false,
    };
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == nix::unistd::getuid().as_raw()
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
    fn is_owned_pid_self() {
        assert!(is_owned_pid(std::process::id()));
    }

    #[test]
    fn is_owned_pid_nonexistent() {
        assert!(!is_owned_pid(u32::MAX));
    }
}
