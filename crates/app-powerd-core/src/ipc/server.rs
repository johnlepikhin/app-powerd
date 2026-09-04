use std::path::Path;
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot, Semaphore};
use tracing::{debug, error, info};

use super::protocol::{self, IpcRequest, IpcResponse};
use crate::engine::EngineEvent;

/// Maximum number of concurrent IPC connections.
const MAX_CONCURRENT_CONNECTIONS: usize = 32;

/// IPC server that listens on a Unix socket and forwards requests to the engine.
pub struct IpcServer {
    listener: UnixListener,
    engine_tx: mpsc::Sender<EngineEvent>,
    max_connections: Arc<Semaphore>,
}

impl IpcServer {
    /// Bind to the socket path.
    pub fn bind(path: &Path, engine_tx: mpsc::Sender<EngineEvent>) -> std::io::Result<Self> {
        // Remove stale socket
        let _ = std::fs::remove_file(path);

        let listener = UnixListener::bind(path)?;

        // Restrict socket permissions to owner only
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;

        info!(path = %path.display(), "IPC server listening");

        Ok(Self {
            listener,
            engine_tx,
            max_connections: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS)),
        })
    }

    /// Run the server, accepting connections and forwarding to the engine.
    pub async fn run(self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let engine_tx = self.engine_tx.clone();
                    let permit = match self.max_connections.clone().acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => break, // semaphore closed
                    };
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, engine_tx).await {
                            debug!(error = %e, "IPC connection error");
                        }
                        drop(permit);
                    });
                }
                Err(e) => {
                    error!(error = %e, "IPC accept error");
                }
            }
        }
    }
}

async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    engine_tx: mpsc::Sender<EngineEvent>,
) -> std::io::Result<()> {
    // Read the frame, then decode. Bailing out on a decode error used to close
    // the socket without a reply, and the client reported that as "daemon not
    // running" — advice that leads the user to kill a daemon which is the only
    // thing able to release the processes it has suspended.
    let raw = match protocol::read_frame(&mut stream).await {
        Ok(raw) => raw,
        Err(e) => return Err(e),
    };
    let request: IpcRequest = match serde_json::from_slice(&raw) {
        Ok(request) => request,
        Err(e) => {
            debug!(error = %e, "IPC request could not be decoded");
            let response = IpcResponse::Error {
                message: format!(
                    "protocol mismatch: this daemon speaks protocol v{} — \
                     the running daemon is probably an older build than the CLI; \
                     restart the daemon",
                    protocol::PROTOCOL_VERSION
                ),
            };
            protocol::write_message(&mut stream, &response).await?;
            return Ok(());
        }
    };
    debug!(?request, "IPC request received");

    let (reply_tx, reply_rx) = oneshot::channel();

    engine_tx
        .send(EngineEvent::IpcRequest {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "engine gone"))?;

    let response = reply_rx.await.unwrap_or(IpcResponse::Error {
        message: "engine did not respond".into(),
    });

    protocol::write_message(&mut stream, &response).await?;
    Ok(())
}
