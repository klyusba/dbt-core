//! Default socket path resolution, shared between server and client.

use std::path::PathBuf;

/// Environment variable that overrides the default socket path.
pub const SOCKET_ENV_VAR: &str = "DBT_DAEMON_SOCKET";

/// Default socket location: `~/.dbt/daemon.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(val) = std::env::var(SOCKET_ENV_VAR) {
        return PathBuf::from(val);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".dbt")
        .join("daemon.sock")
}
