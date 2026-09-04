use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

use super::cgroup_base_path;
use super::systemd_dbus::SystemdManager;
use crate::error::SystemError;

/// CPU period in microseconds for cgroup cpu.max (100ms).
const CPU_PERIOD_US: u32 = 100_000;

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

impl std::fmt::Display for CgroupCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SystemdTransient => write!(f, "SystemdTransient"),
            Self::DirectWrite => write!(f, "DirectWrite"),
            Self::SignalOnly => write!(f, "SignalOnly"),
        }
    }
}

/// What the kernel actually lets this daemon do, established once at startup.
///
/// Being able to create a sub-cgroup does not imply being able to use it: the
/// `cpu` controller has to be enabled in the parent's `subtree_control`, which
/// on a system without delegation it is not. Probing only the directory creation
/// is why `cpu_weight` and `cpu_quota` could be configured, accepted, and then
/// silently do nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CgroupCapabilities {
    pub tier: CgroupCapability,
    /// `cgroup.freeze` is usable, so freezing need not fall back to SIGSTOP.
    pub freezer: bool,
    /// The `cpu` controller is delegated, so `cpu.weight` / `cpu.max` work.
    pub cpu_control: bool,
}

/// Manages cgroup v2 operations.
pub struct CgroupManager {
    capabilities: CgroupCapabilities,
    base_path: PathBuf,
    systemd: Option<SystemdManager>,
}

impl Default for CgroupManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupManager {
    pub fn capability(&self) -> CgroupCapability {
        self.capabilities.tier
    }

    pub fn capabilities(&self) -> CgroupCapabilities {
        self.capabilities
    }

    /// Whether the cgroup freezer can be used instead of signals.
    pub fn supports_freezer(&self) -> bool {
        self.capabilities.freezer
    }

    /// Whether `cpu.weight` / `cpu.max` are actually enforced.
    pub fn supports_cpu_control(&self) -> bool {
        self.capabilities.cpu_control
    }

    /// Detect available cgroup capability.
    pub fn new() -> Self {
        let base_path = cgroup_base_path();

        // Try DirectWrite first.
        if base_path.exists() {
            let probe = base_path.join("app-powerd-probe");
            let _ = fs::remove_dir(&probe);
            if fs::create_dir(&probe).is_ok() {
                let capabilities = CgroupCapabilities {
                    tier: CgroupCapability::DirectWrite,
                    freezer: probe.join("cgroup.freeze").exists(),
                    cpu_control: controller_enabled(&base_path, "cpu"),
                };
                let _ = fs::remove_dir(&probe);
                return Self {
                    capabilities,
                    base_path,
                    systemd: None,
                };
            }
        }

        // Try systemd transient scopes.
        if let Some(mgr) = SystemdManager::try_connect() {
            return Self {
                capabilities: CgroupCapabilities {
                    tier: CgroupCapability::SystemdTransient,
                    freezer: true,
                    cpu_control: true,
                },
                base_path,
                systemd: Some(mgr),
            };
        }

        Self {
            capabilities: CgroupCapabilities {
                tier: CgroupCapability::SignalOnly,
                freezer: false,
                cpu_control: false,
            },
            base_path,
            systemd: None,
        }
    }

    /// Report the detected capabilities exactly once, at startup.
    ///
    /// Degraded operation is a warning because it silently changes what the
    /// configuration means: without the `cpu` controller the `cpu_weight` and
    /// `cpu_quota` of every throttle profile are inert, and the daemon falls
    /// back to signals for freezing. Previously this was an `info!` line about
    /// an enum name, which told the operator nothing about the consequences.
    pub fn report_capabilities(&self, cpu_profiles: &[String]) {
        let caps = self.capabilities;
        if caps.tier == CgroupCapability::SignalOnly {
            warn!(
                mode = %caps.tier,
                "cgroup delegation unavailable: falling back to SIGSTOP/SIGCONT. \
                 cpu_weight and cpu_quota will NOT be applied; throttling is limited to nice. \
                 This is expected on systems without cgroup v2 delegation to the user session."
            );
        } else if !caps.cpu_control {
            warn!(
                mode = %caps.tier,
                "cgroup subtree available but the cpu controller is not delegated: \
                 cpu_weight and cpu_quota will NOT be applied; throttling is limited to nice."
            );
        } else {
            info!(mode = %caps.tier, freezer = caps.freezer, "cgroup capability detected");
        }

        if !caps.cpu_control && !cpu_profiles.is_empty() {
            warn!(
                profiles = %cpu_profiles.join(", "),
                "these profiles declare cpu_weight/cpu_quota that cannot be enforced in the current mode"
            );
        }
    }

    /// Create a cgroup for an application with initial PIDs.
    ///
    /// - **DirectWrite**: creates the directory; PIDs are moved separately by the caller.
    /// - **SystemdTransient**: creates a transient scope with PIDs via D-Bus.
    /// - **SignalOnly**: returns an error (no cgroup support).
    pub fn create_cgroup(&self, name: &str, pids: &[u32]) -> Result<PathBuf, SystemError> {
        match self.capabilities.tier {
            CgroupCapability::DirectWrite => {
                let sanitized = super::sanitize_unit_name(name);
                let path = self.base_path.join(format!("app-powerd-{sanitized}"));
                if !path.exists() {
                    fs::create_dir_all(&path).map_err(|e| SystemError::CgroupOperation {
                        operation: "create".into(),
                        path: path.display().to_string(),
                        source: e,
                    })?;
                    debug!(path = %path.display(), "created cgroup");
                }
                Ok(path)
            }
            CgroupCapability::SystemdTransient => {
                if let Some(ref mgr) = self.systemd {
                    mgr.start_transient_scope(name, pids, &format!("app-powerd managed: {name}"))
                } else {
                    Err(SystemError::CgroupError {
                        message: "systemd manager not available".into(),
                    })
                }
            }
            CgroupCapability::SignalOnly => Err(SystemError::NoCgroupCapability),
        }
    }

    /// Move a PID into a cgroup.
    pub fn move_pid(&self, cgroup_path: &Path, pid: u32) -> Result<(), SystemError> {
        let procs_file = cgroup_path.join("cgroup.procs");
        fs::write(&procs_file, pid.to_string()).map_err(|e| SystemError::CgroupOperation {
            operation: format!("move_pid({pid})"),
            path: procs_file.display().to_string(),
            source: e,
        })?;
        debug!(pid, cgroup = %cgroup_path.display(), "moved pid to cgroup");
        Ok(())
    }

    /// Freeze a cgroup (cgroup v2 freezer).
    pub fn freeze(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        let freeze_file = cgroup_path.join("cgroup.freeze");
        fs::write(&freeze_file, "1").map_err(|e| SystemError::CgroupOperation {
            operation: "freeze".into(),
            path: freeze_file.display().to_string(),
            source: e,
        })?;
        debug!(cgroup = %cgroup_path.display(), "cgroup frozen");
        Ok(())
    }

    /// Thaw (unfreeze) a cgroup.
    pub fn thaw(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        let freeze_file = cgroup_path.join("cgroup.freeze");
        fs::write(&freeze_file, "0").map_err(|e| SystemError::CgroupOperation {
            operation: "thaw".into(),
            path: freeze_file.display().to_string(),
            source: e,
        })?;
        debug!(cgroup = %cgroup_path.display(), "cgroup thawed");
        Ok(())
    }

    /// Set cpu.weight for throttling.
    pub fn set_cpu_weight(&self, cgroup_path: &Path, weight: u32) -> Result<(), SystemError> {
        let file = cgroup_path.join("cpu.weight");
        fs::write(&file, weight.to_string()).map_err(|e| SystemError::CgroupOperation {
            operation: "set_cpu_weight".into(),
            path: file.display().to_string(),
            source: e,
        })?;
        debug!(weight, cgroup = %cgroup_path.display(), "set cpu.weight");
        Ok(())
    }

    /// Set cpu.max for quota limiting. Format: "quota period" (microseconds).
    pub fn set_cpu_max(&self, cgroup_path: &Path, quota_pct: &str) -> Result<(), SystemError> {
        // Parse "40%" → "40000 100000"
        let pct: u32 =
            quota_pct
                .trim_end_matches('%')
                .parse()
                .map_err(|_| SystemError::CgroupError {
                    message: format!("invalid cpu quota: {quota_pct}"),
                })?;
        if pct == 0 || pct > 100 {
            return Err(SystemError::CgroupError {
                message: format!("cpu_quota must be 1-100%, got {pct}%"),
            });
        }
        let period = CPU_PERIOD_US;
        let quota = period * pct / 100;
        let value = format!("{quota} {period}");

        let file = cgroup_path.join("cpu.max");
        fs::write(&file, &value).map_err(|e| SystemError::CgroupOperation {
            operation: "set_cpu_max".into(),
            path: file.display().to_string(),
            source: e,
        })?;
        debug!(value, cgroup = %cgroup_path.display(), "set cpu.max");
        Ok(())
    }

    /// Reset cpu controls to defaults.
    ///
    /// Returns nothing on purpose: the previous signature promised a `Result`
    /// that was always `Ok`, which made callers believe they were checking
    /// something they were not.
    pub fn reset_cpu(&self, cgroup_path: &Path) {
        let weight_err = fs::write(cgroup_path.join("cpu.weight"), "100").err();
        let max_err = fs::write(cgroup_path.join("cpu.max"), format!("max {CPU_PERIOD_US}")).err();
        if let Some(e) = &weight_err {
            debug!(cgroup = %cgroup_path.display(), error = %e, "failed to reset cpu.weight");
        }
        if let Some(e) = &max_err {
            debug!(cgroup = %cgroup_path.display(), error = %e, "failed to reset cpu.max");
        }
    }

    /// Remove stale app-powerd cgroups left over from a previous run.
    pub fn cleanup_stale_cgroups(&self) {
        if self.capabilities.tier == CgroupCapability::SignalOnly {
            return;
        }
        let Ok(entries) = fs::read_dir(&self.base_path) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with("app-powerd-") {
                let path = entry.path();
                // Try thaw before removing (frozen cgroup can't be removed)
                let _ = fs::write(path.join("cgroup.freeze"), "0");
                match fs::remove_dir(&path) {
                    Ok(()) => info!(path = %path.display(), "cleaned up stale cgroup"),
                    Err(e) => {
                        debug!(path = %path.display(), error = %e, "failed to clean up stale cgroup")
                    }
                }
            }
        }
    }

    /// Remove a cgroup (must be empty).
    pub fn remove_cgroup(&self, cgroup_path: &Path) -> Result<(), SystemError> {
        if cgroup_path.exists() {
            fs::remove_dir(cgroup_path).map_err(|e| SystemError::CgroupOperation {
                operation: "remove".into(),
                path: cgroup_path.display().to_string(),
                source: e,
            })?;
        }
        Ok(())
    }
}

/// Whether `controller` is enabled for children of `path`.
///
/// A controller listed in a cgroup's `cgroup.subtree_control` is the only thing
/// that makes its interface files appear in child cgroups. Without this check a
/// sub-cgroup can be created successfully and still have no `cpu.max` to write.
fn controller_enabled(path: &Path, controller: &str) -> bool {
    fs::read_to_string(path.join("cgroup.subtree_control"))
        .map(|content| content.split_whitespace().any(|c| c == controller))
        .unwrap_or(false)
}
