use serde::{Deserialize, Serialize};

use crate::metrics::MetricsSnapshot;
use crate::system::power::PowerSource;

/// IPC request from CLI to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
#[non_exhaustive]
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
    },
    Stats {
        metrics: MetricsSnapshot,
    },
}

/// Serializable app info for IPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppInfo {
    pub app_id: String,
    pub state: crate::state::AppState,
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

/// Read a length-prefixed JSON message with a 10-second timeout.
pub async fn read_message<T: serde::de::DeserializeOwned>(
    stream: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> std::io::Result<T> {
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

        let request = IpcRequest::Freeze { pid: 42 };
        write_message(&mut client_write, &request).await.unwrap();
        drop(client_write); // close write side

        let decoded: IpcRequest = read_message(&mut server).await.unwrap();
        match decoded {
            IpcRequest::Freeze { pid } => assert_eq!(pid, 42),
            _ => panic!("unexpected variant: {decoded:?}"),
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
