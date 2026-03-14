use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file not found: {path}")]
    NotFound { path: PathBuf },

    #[error("parse error: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("validation error: {message}")]
    Validation { message: String },

    #[error("invalid regex in rule '{rule_id}': {source}")]
    InvalidRegex {
        rule_id: String,
        source: regex::Error,
    },

    #[error("unknown profile '{profile}' in rule '{rule_id}'")]
    UnknownProfile { rule_id: String, profile: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    #[error("process {pid} not found")]
    ProcessNotFound { pid: u32 },

    #[error("cgroup operation failed: {message}")]
    CgroupError { message: String },

    #[error("no cgroup capability available")]
    NoCgroupCapability,

    #[error("freeze failed for {app_id}: {reason}")]
    FreezeFailed { app_id: String, reason: String },

    #[error("throttle failed for {app_id}: {reason}")]
    ThrottleFailed { app_id: String, reason: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("nix errno: {0}")]
    Nix(#[from] nix::errno::Errno),
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("no display server detected")]
    NoDisplayServer,

    #[error("x11 connection failed: {0}")]
    X11Connection(String),

    #[error("wayland connection failed: {0}")]
    WaylandConnection(String),

    #[error("backend disconnected")]
    Disconnected,
}

