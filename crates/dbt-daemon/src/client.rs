//! Thin client that forwards a dbt command to a running daemon and returns its
//! exit code.

use std::path::Path;

use tokio::net::UnixStream;

use crate::protocol::{DaemonRequest, DaemonResponse, read_frame, write_frame};

/// Connect to the daemon at `socket_path`, send `args` and `cwd`, and return
/// the exit code reported by the daemon.
///
/// Returns `Err` if the socket cannot be reached (daemon not running, etc.).
pub async fn send_command(
    socket_path: &Path,
    args: Vec<String>,
    cwd: Option<String>,
) -> std::io::Result<i32> {
    let mut stream = UnixStream::connect(socket_path).await?;

    let request = DaemonRequest { args, cwd };
    write_frame(&mut stream, &request).await?;

    let response: Option<DaemonResponse> = read_frame(&mut stream).await?;
    match response {
        Some(r) => Ok(r.exit_code),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "daemon closed connection without sending a response",
        )),
    }
}
