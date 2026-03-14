use std::path::Path;

use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use super::protocol::{self, IpcRequest, IpcResponse};
use crate::engine::EngineEvent;

/// IPC server that listens on a Unix socket and forwards requests to the engine.
pub struct IpcServer {
    listener: UnixListener,
    engine_tx: mpsc::Sender<EngineEvent>,
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
        })
    }

    /// Run the server, accepting connections and forwarding to the engine.
    pub async fn run(self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, _)) => {
                    let engine_tx = self.engine_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, engine_tx).await {
                            debug!(error = %e, "IPC connection error");
                        }
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
    let request: IpcRequest = protocol::read_message(&mut stream).await?;
    debug!(?request, "IPC request received");

    let (reply_tx, reply_rx) = oneshot::channel();

    engine_tx
        .send(EngineEvent::IpcRequest {
            request,
            reply: reply_tx,
        })
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "engine gone"))?;

    let response = reply_rx
        .await
        .unwrap_or(IpcResponse::Error {
            message: "engine did not respond".into(),
        });

    protocol::write_message(&mut stream, &response).await?;
    Ok(())
}
