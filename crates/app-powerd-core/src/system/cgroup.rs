use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, info};

use super::systemd_dbus::SystemdManager;
use super::{sanitize_unit_name, cgroup_base_path};
use crate::error::SystemError;

/// Cgroup capability tier (auto-detected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupCapability {
    /// systemd transient scopes via D-Bus.
    SystemdTransient,
    /// Direct write to delegated cgroup subtree.
    DirectWrite,
    /// No cgroup access — use SIGSTOP/SIGCONT.
    SignalOnly,
}

/// Manages cgroup v2 operations.
pub struct CgroupManager {
    pub capability: CgroupCapability,
    base_path: PathBuf,
    systemd: Option<SystemdManager>,
}

impl Default for CgroupManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupManager {
    /// Detect available cgroup capability.
    pub fn new() -> Self {
        let base_path = cgroup_base_path();

        // Try DirectWrite first
        if base_path.exists() {
            let test_path = base_path.join("app-powerd-probe");
            if fs::create_dir(&test_path).is_ok() {
                let _ = fs::remove_dir(&test_path);
                info!(capability = ?CgroupCapability::DirectWrite, "cgroup capability detected");
                return Self {
                    capability: CgroupCapability::DirectWrite,
                    base_path,
                    systemd: None,
                };
            }
        }

        // Try systemd transient
        if let Some(mgr) = SystemdManager::try_connect() {
            info!(capability = ?CgroupCapability::SystemdTransient, "cgroup capability detected");
            return Self {
                capability: CgroupCapability::SystemdTransient,
                base_path,
                systemd: Some(mgr),
            };
        }

        info!(capability = ?CgroupCapability::SignalOnly, "cgroup capability detected");
        Self {
            capability: CgroupCapability::SignalOnly,
            base_path,
            systemd: None,
        }
    }

    /// Create a cgroup for an application.
    pub fn create_cgroup(&self, name: &str) -> Result<PathBuf, SystemError> {
        match self.capability {
            CgroupCapability::DirectWrite => {
                let path = self.base_path.join(format!("app-powerd-{name}"));
                if !path.exists() {
                    fs::create_dir_all(&path).map_err(|e| SystemError::CgroupError {
                        message: format!("failed to create cgroup {}: {e}", path.display()),
                    })?;
                    debug!(path = %path.display(), "created cgroup");
                }
                Ok(path)
            }
            CgroupCapability::SystemdTransient => {
                // PIDs will be moved later via move_pid; create scope with empty PIDs
                // and let the caller move PIDs into the resulting cgroup path.
                // Actually, we need at least one PID for the scope — defer to
                // create_cgroup_with_pids.
                let path = self.base_path.join(format!("app-powerd-{}.scope", sanitize_unit_name(name)));
                Ok(path)
            }
            CgroupCapability::SignalOnly => Err(SystemError::NoCgroupCapability),
        }
    }

    /// Create a cgroup for an application with initial PIDs (used for systemd transient).
    pub fn create_cgroup_with_pids(&self, name: &str, pids: &[u32]) -> Result<PathBuf, SystemError> {
        match self.capability {
            CgroupCapability::SystemdTransient => {
                if let Some(ref mgr) = self.systemd {
                    mgr.start_transient_scope(name, pids, &format!("app-powerd managed: {name}"))
                } else {
                    Err(SystemError::CgroupError {
                        message: "systemd manager not available".into(),
                    })
                }
            }
            _ => self.create_cgroup(name),
        }
    }

    /// Move a PID into a cgroup.
    pub fn move_pid(&self, cgroup_path: &Path, pid: u32) -> Result<(), SystemError> {
        let procs_file = cgroup_path.join("cgroup.procs");
        fs::write(&procs_file, pid.to_string()).map_err(|e| SystemError::CgroupError {
            message: format!("failed to move pid {pid} to {}: {e}", cgroup_path.display()),
        })?;
        debug!(pid, cgroup = %cgroup_path.display(), "moved pid to cgroup");
        Ok(())
    }

    /// Freeze a cgroup (cgroup v2 freezer).
    pub fn freeze(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        let freeze_file = cgroup_path.join("cgroup.freeze");
        fs::write(&freeze_file, "1").map_err(|e| SystemError::CgroupError {
            message: format!("failed to freeze {}: {e}", cgroup_path.display()),
        })?;
        debug!(cgroup = %cgroup_path.display(), "cgroup frozen");
        Ok(())
    }

    /// Thaw (unfreeze) a cgroup.
    pub fn thaw(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        let freeze_file = cgroup_path.join("cgroup.freeze");
        fs::write(&freeze_file, "0").map_err(|e| SystemError::CgroupError {
            message: format!("failed to thaw {}: {e}", cgroup_path.display()),
        })?;
        debug!(cgroup = %cgroup_path.display(), "cgroup thawed");
        Ok(())
    }

    /// Set cpu.weight for throttling.
    pub fn set_cpu_weight(&self, cgroup_path: &Path, weight: u32) -> Result<(), SystemError> {
        let file = cgroup_path.join("cpu.weight");
        fs::write(&file, weight.to_string()).map_err(|e| SystemError::CgroupError {
            message: format!("failed to set cpu.weight: {e}"),
        })?;
        debug!(weight, cgroup = %cgroup_path.display(), "set cpu.weight");
        Ok(())
    }

    /// Set cpu.max for quota limiting. Format: "quota period" (microseconds).
    pub fn set_cpu_max(&self, cgroup_path: &Path, quota_pct: &str) -> Result<(), SystemError> {
        // Parse "40%" → "40000 100000"
        let pct: u32 = quota_pct
            .trim_end_matches('%')
            .parse()
            .map_err(|_| SystemError::CgroupError {
                message: format!("invalid cpu quota: {quota_pct}"),
            })?;
        let period = 100_000u32; // 100ms
        let quota = period * pct / 100;
        let value = format!("{quota} {period}");

        let file = cgroup_path.join("cpu.max");
        fs::write(&file, &value).map_err(|e| SystemError::CgroupError {
            message: format!("failed to set cpu.max: {e}"),
        })?;
        debug!(value, cgroup = %cgroup_path.display(), "set cpu.max");
        Ok(())
    }

    /// Reset cpu controls to defaults.
    pub fn reset_cpu(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        if let Err(e) = fs::write(cgroup_path.join("cpu.weight"), "100") {
            debug!(cgroup = %cgroup_path.display(), error = %e, "failed to reset cpu.weight");
        }
        if let Err(e) = fs::write(cgroup_path.join("cpu.max"), "max 100000") {
            debug!(cgroup = %cgroup_path.display(), error = %e, "failed to reset cpu.max");
        }
        Ok(())
    }

    /// Remove stale app-powerd cgroups left over from a previous run.
    pub fn cleanup_stale_cgroups(&self) {
        if self.capability == CgroupCapability::SignalOnly {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.base_path) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else { continue };
            if name_str.starts_with("app-powerd-") {
                let path = entry.path();
                // Try thaw before removing (frozen cgroup can't be removed)
                let _ = fs::write(path.join("cgroup.freeze"), "0");
                match fs::remove_dir(&path) {
                    Ok(()) => info!(path = %path.display(), "cleaned up stale cgroup"),
                    Err(e) => debug!(path = %path.display(), error = %e, "failed to clean up stale cgroup"),
                }
            }
        }
    }

    /// Remove a cgroup (must be empty).
    pub fn remove_cgroup(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        if cgroup_path.exists() {
            fs::remove_dir(cgroup_path).map_err(|e| SystemError::CgroupError {
                message: format!("failed to remove cgroup: {e}"),
            })?;
        }
        Ok(())
    }
}
