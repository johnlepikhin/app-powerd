use std::path::PathBuf;

/// Errors related to configuration loading, parsing, and validation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The configuration file does not exist at the expected path.
    #[error("config file not found: {path}")]
    NotFound { path: PathBuf },

    /// YAML parsing failed.
    #[error("parse error: {0}")]
    Parse(#[from] serde_yaml_ng::Error),

    /// Semantic validation error (e.g. invalid field values).
    #[error("validation error: {message}")]
    Validation { message: String },

    /// A regex in a rule's match criteria failed to compile.
    #[error("invalid regex in rule '{rule_id}': {source}")]
    InvalidRegex {
        rule_id: String,
        source: regex::Error,
    },

    /// A rule references a profile that is not defined.
    #[error("unknown profile '{profile}' in rule '{rule_id}'")]
    UnknownProfile { rule_id: String, profile: String },

    /// Filesystem I/O error while reading the config file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors from system-level operations: processes, cgroups, signals.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SystemError {
    /// The target process does not exist in /proc.
    #[error("process {pid} not found")]
    ProcessNotFound { pid: u32 },

    /// Failed to read process metadata from /proc.
    #[error("process {pid}: {message}")]
    ProcessReadError { pid: u32, message: String },

    /// Generic cgroup error with a descriptive message.
    #[error("cgroup operation failed: {message}")]
    CgroupError { message: String },

    /// A specific cgroup filesystem operation failed.
    #[error("cgroup {operation} failed on {path}: {source}")]
    CgroupOperation {
        operation: String,
        path: String,
        source: std::io::Error,
    },

    /// No cgroup capability was detected (DirectWrite, SystemdTransient).
    #[error("no cgroup capability available")]
    NoCgroupCapability,

    /// Throttle operation failed for an application.
    #[error("throttle failed for {app_id}: {reason}")]
    ThrottleFailed { app_id: String, reason: String },

    /// Filesystem I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Unix signal or errno error from the nix crate.
    #[error("nix errno: {0}")]
    Nix(#[from] nix::errno::Errno),
}

/// Errors from desktop/display-server integration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DesktopError {
    /// Neither X11 nor Wayland display server was detected.
    #[error(
        "no supported display server detected (WAYLAND_DISPLAY and DISPLAY are unset or \
         unavailable); ensure the daemon runs inside a desktop session and that the appropriate \
         feature flag (x11 or wayland) was enabled at build time"
    )]
    NoDisplayServer,

    /// X11 connection or protocol error.
    #[error("x11 connection failed: {0}")]
    X11Connection(String),

    /// Wayland connection or protocol error.
    #[error("wayland connection failed: {0}")]
    WaylandConnection(String),

    /// The focus backend disconnected unexpectedly.
    #[error("backend disconnected")]
    Disconnected,
}
