use std::path::Path;

use tokio::net::UnixStream;

use super::protocol::{self, IpcRequest, IpcResponse};

/// Send an IPC request to the daemon and receive a response.
pub async fn send_request(
    socket_path: &Path,
    request: IpcRequest,
) -> std::io::Result<IpcResponse> {
    let mut stream = UnixStream::connect(socket_path).await?;
    protocol::write_message(&mut stream, &request).await?;
    protocol::read_message(&mut stream).await
}
