use serde::{Deserialize, Serialize};

use crate::metrics::MetricsSnapshot;

/// IPC request from CLI to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcRequest {
    /// List all tracked apps.
    List,
    /// Get daemon status.
    Status,
    /// Get metrics.
    Stats,
    /// Force freeze a PID.
    Freeze { pid: u32 },
    /// Force thaw a PID.
    Thaw { pid: u32 },
    /// Reload configuration.
    ReloadConfig,
    /// Shutdown the daemon.
    Shutdown,
}

/// IPC response from daemon to CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
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
        power_source: String,
        tracked_apps: usize,
        uptime_secs: u64,
    },
    Stats {
        metrics: MetricsSnapshot,
    },
}

/// Serializable app info for IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub app_id: String,
    pub state: String,
    pub pids: Vec<u32>,
    pub executable: Option<String>,
    pub wm_class: Option<String>,
    pub window_title: Option<String>,
}

/// Socket path for IPC.
pub fn socket_path() -> std::path::PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", nix::unistd::getuid().as_raw()));
    std::path::PathBuf::from(runtime_dir).join("app-powerd.sock")
}

/// Length-prefixed message framing: 4 bytes u32 BE + JSON payload.
pub async fn write_message(
    stream: &mut (impl tokio::io::AsyncWriteExt + Unpin),
    msg: &impl Serialize,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&json).await?;
    stream.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON message with a 10-second timeout.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    stream: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> std::io::Result<T> {
    use std::time::Duration;
    use tokio::time::timeout;

    const READ_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_MESSAGE_SIZE: usize = 65_536;

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
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
