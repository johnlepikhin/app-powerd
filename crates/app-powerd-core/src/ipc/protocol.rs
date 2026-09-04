use serde::{Deserialize, Serialize};

use crate::metrics::MetricsSnapshot;
use crate::system::power::PowerSource;

/// Version of the request/response protocol spoken by this build.
///
/// Reported in [`IpcResponse::Status`] so a mismatch between a freshly installed
/// binary and a still-running daemon can be named rather than presenting itself
/// as an unexplained failure.
pub const PROTOCOL_VERSION: u32 = 2;

/// What a freeze/thaw command applies to.
///
/// Untagged and flattened into the request, so the `pid` form is byte-for-byte
/// the 1.x wire format: a 1.x client keeps working against a 2.x daemon during
/// the window between installing a new binary and restarting the old daemon.
///
/// Because the representation is untagged, a request carrying **both** `pid` and
/// `app` is not rejected: variants are tried in declaration order and unknown
/// fields are ignored, so `pid` wins and `app` is dropped. This is accepted
/// deliberately — the alternative is a tagged form that breaks the 1.x wire
/// compatibility above — and pinned by `pid_wins_when_both_fields_present`.
/// Clients must send exactly one of the two.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Target {
    /// A single process.
    Pid { pid: u32 },
    /// Every process of a tracked application, by app id (case-insensitive).
    App { app: String },
}

impl Target {
    /// Target a single process by PID.
    pub fn pid(pid: u32) -> Self {
        Self::Pid { pid }
    }

    /// Target every process of a tracked application by app id.
    ///
    /// The id is matched case-insensitively by the daemon.
    pub fn app(app: impl Into<String>) -> Self {
        Self::App { app: app.into() }
    }

    /// Parse a CLI argument: all digits means a PID, anything else an app name.
    pub fn parse(arg: &str) -> Self {
        match arg.parse::<u32>() {
            Ok(pid) => Self::pid(pid),
            Err(_) => Self::app(arg),
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pid { pid } => write!(f, "pid {pid}"),
            Self::App { app } => write!(f, "app {app}"),
        }
    }
}

/// IPC request from CLI to daemon.
///
/// Deliberately **not** `deny_unknown_fields`: rejecting an unknown field made a
/// newer client talking to an older daemon look like the daemon was not running
/// at all, which invites the user to kill a daemon that is holding processes
/// suspended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum IpcRequest {
    /// List all tracked apps.
    List,
    /// Get daemon status.
    Status,
    /// Get metrics.
    Stats,
    /// Force freeze a target.
    Freeze {
        #[serde(flatten)]
        target: Target,
    },
    /// Force thaw a target.
    Thaw {
        #[serde(flatten)]
        target: Target,
    },
    /// Release every process the daemon has suspended.
    ThawAll,
    /// Reload configuration.
    ReloadConfig,
    /// Override the detected power source. `None` clears the override (auto mode).
    ///
    /// The override is in-memory only and is reset on daemon restart.
    /// `Ok` means the command was accepted and the override is set; the actual
    /// thaw/start side-effects on tracked apps may partially fail and are only
    /// reported in the daemon log (search for `power source override updated`).
    SetPowerOverride { source: Option<PowerSource> },
    /// Shutdown the daemon.
    Shutdown,
}

/// IPC response from daemon to CLI.
///
/// Note: variants and fields are intentionally not `deny_unknown_fields` so
/// that newer daemons can add response fields without breaking older clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[non_exhaustive]
pub enum IpcResponse {
    Ok {
        message: String,
    },
    Error {
        message: String,
    },
    AppList {
        apps: Vec<AppInfo>,
    },
    Status {
        enabled: bool,
        power_source: PowerSource,
        #[serde(default)]
        forced_power_source: Option<PowerSource>,
        tracked_apps: usize,
        uptime_secs: u64,
        /// Which suspension mechanism is actually in use.
        #[serde(default)]
        cgroup_mode: String,
        /// Whether `cpu_weight` / `cpu_quota` are enforced at all.
        #[serde(default)]
        cpu_control: bool,
        /// Applications the built-in deny-list is refusing to manage.
        #[serde(default)]
        protected_apps: usize,
        #[serde(default)]
        protocol_version: u32,
    },
    Stats {
        metrics: MetricsSnapshot,
    },
}

/// Serializable app info for IPC.
///
/// Not `deny_unknown_fields`, matching the promise made above `IpcResponse`:
/// an older client must be able to read a newer daemon's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub app_id: String,
    pub state: crate::state::AppState,
    pub pids: Vec<u32>,
    pub executable: Option<String>,
    pub wm_class: Option<String>,
    pub window_title: Option<String>,
    /// Profile the matched rule referenced, if any.
    #[serde(default)]
    pub profile: Option<String>,
    /// Id of the rule that decided this application's policy.
    #[serde(default)]
    pub rule_id: Option<String>,
    /// Seconds spent in the current state.
    #[serde(default)]
    pub state_since_secs: u64,
    /// Why this application is exempt from management, if it is.
    #[serde(default)]
    pub protected: Option<String>,
}

/// Socket path for IPC.
pub fn socket_path() -> std::path::PathBuf {
    crate::system::runtime_dir().join("app-powerd.sock")
}

/// Length-prefixed message framing: 4 bytes u32 BE + JSON payload.
pub async fn write_message(
    stream: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    msg: &impl Serialize,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let msg_len: u32 = json.len().try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "message too large for u32 length prefix",
        )
    })?;
    let len = msg_len.to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;
    Ok(())
}

/// Timeout for reading a single IPC message.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Maximum allowed IPC message size (64 KiB).
const MAX_MESSAGE_SIZE: usize = 65_536;

/// Read one length-prefixed frame without interpreting it.
///
/// Separate from [`read_message`] so a decode failure can still be answered on
/// the same connection instead of dropping it silently.
pub async fn read_frame(
    stream: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> std::io::Result<Vec<u8>> {
    use tokio::time::timeout;

    let mut len_buf = [0u8; 4];
    timeout(READ_TIMEOUT, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))??;
    let len = u32::from_be_bytes(len_buf) as usize;

    if len > MAX_MESSAGE_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message too large",
        ));
    }

    let mut buf = vec![0u8; len];
    timeout(READ_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))??;
    Ok(buf)
}

/// Read a length-prefixed JSON message with a 10-second timeout.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    stream: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> std::io::Result<T> {
    let buf = read_frame(stream).await?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_ipc_message() {
        let (client, mut server) = tokio::io::duplex(1024);
        let (_client_read, mut client_write) = tokio::io::split(client);

        let request = IpcRequest::Freeze {
            target: Target::pid(42),
        };
        write_message(&mut client_write, &request).await.unwrap();
        drop(client_write); // close write side

        let decoded: IpcRequest = read_message(&mut server).await.unwrap();
        match decoded {
            IpcRequest::Freeze {
                target: Target::Pid { pid },
            } => assert_eq!(pid, 42),
            _ => panic!("unexpected variant: {decoded:?}"),
        }
    }

    /// The 1.x wire form must still be understood, so a newly installed CLI
    /// talking to a not-yet-restarted daemon gets an answer instead of looking
    /// like the daemon is absent.
    #[test]
    fn legacy_pid_form_is_accepted() {
        let legacy = br#"{"type":"Freeze","pid":42}"#;
        let decoded: IpcRequest = serde_json::from_slice(legacy).unwrap();
        assert!(matches!(
            decoded,
            IpcRequest::Freeze {
                target: Target::Pid { pid: 42 }
            }
        ));
    }

    /// ...and we must keep emitting that form for a plain PID target.
    #[test]
    fn pid_target_serializes_to_legacy_shape() {
        let json = serde_json::to_value(IpcRequest::Thaw {
            target: Target::pid(7),
        })
        .unwrap();
        assert_eq!(json["type"], "Thaw");
        assert_eq!(json["pid"], 7);
    }

    /// A request carrying both fields is ambiguous by construction (see the
    /// note on [`Target`]). Pin the resolution: `pid` wins, `app` is ignored.
    #[test]
    fn pid_wins_when_both_fields_present() {
        let both = br#"{"type":"Freeze","app":"Firefox","pid":1}"#;
        let decoded: IpcRequest = serde_json::from_slice(both).unwrap();
        assert!(matches!(
            decoded,
            IpcRequest::Freeze {
                target: Target::Pid { pid: 1 }
            }
        ));
    }

    #[test]
    fn app_target_roundtrips() {
        let request = IpcRequest::Thaw {
            target: Target::app("Firefox"),
        };
        let json = serde_json::to_vec(&request).unwrap();
        let decoded: IpcRequest = serde_json::from_slice(&json).unwrap();
        match decoded {
            IpcRequest::Thaw {
                target: Target::App { app },
            } => assert_eq!(app, "Firefox"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_oversized_message() {
        let (client, mut server) = tokio::io::duplex(1024);
        let (mut _client_read, mut client_write) = tokio::io::split(client);

        // Write a length header claiming 100KB (exceeds MAX_MESSAGE_SIZE)
        use tokio::io::AsyncWriteExt;
        let len = (100_000u32).to_be_bytes();
        client_write.write_all(&len).await.unwrap();
        drop(client_write);

        let result: std::io::Result<IpcRequest> = read_message(&mut server).await;
        assert!(result.is_err());
    }
}
