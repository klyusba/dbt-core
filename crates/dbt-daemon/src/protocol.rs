//! Wire protocol for daemon ↔ client communication.
//!
//! Transport: Unix domain socket.
//! Framing:   4-byte big-endian payload length followed by UTF-8 JSON bytes.
//!
//! Request  (client → daemon): DaemonRequest
//! Response (daemon → client): DaemonResponse

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A command forwarded from the client to the daemon.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonRequest {
    /// Full argv (including the program name as argv[0], e.g. `["dbt", "run", "--select", "foo"]`).
    pub args: Vec<String>,
    /// Working directory of the caller.  Sent so the daemon can resolve relative
    /// paths the same way the original process would.  If absent the daemon uses
    /// its own cwd, which means `--project-dir` must be explicit in `args`.
    pub cwd: Option<String>,
}

/// Result sent back from the daemon after the command completes.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonResponse {
    /// Process exit code (0 = success).
    pub exit_code: i32,
}

/// Write a length-prefixed JSON frame to the stream.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON frame from the stream and deserialize it.
/// Returns `None` on clean EOF.
pub async fn read_frame<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncReadExt + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let value = serde_json::from_slice(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}
