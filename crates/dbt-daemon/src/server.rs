//! Unix socket server: accepts connections and dispatches dbt commands.
//!
//! Requests are processed **serially** — one at a time.  This mirrors the way
//! dbt is normally used and avoids races on global process state (rustls
//! provider installation, metrics counters, cwd).

use std::path::PathBuf;
use std::sync::Arc;

use dbt_clap_core::{CliParserFactory as _, from_main};
use dbt_common::cancellation::never_cancels;
use dbt_features::cli::DefaultCliParserFactory;
use dbt_features::feature_stack::FeatureStack;
use dbt_lib::dbt_lib::execute_fs_and_shutdown;
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, info};

use crate::protocol::{DaemonRequest, DaemonResponse, read_frame, write_frame};

/// Bind a Unix socket at `socket_path` and serve requests forever.
///
/// Each accepted connection receives exactly one [`DaemonRequest`] and sends
/// back exactly one [`DaemonResponse`].  The connection is then closed and the
/// server waits for the next client.
pub async fn run_server(socket_path: PathBuf, feature_stack: Arc<FeatureStack>) {
    // Remove a stale socket file from a previous daemon run.
    let _ = std::fs::remove_file(&socket_path);

    // Ensure the parent directory exists (e.g. ~/.dbt/).
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("failed to create socket directory {}: {e}", parent.display());
            return;
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("failed to bind socket {}: {e}", socket_path.display());
            return;
        }
    };

    info!("dbt-daemon listening on {}", socket_path.display());

    // Build one shared CliParser; it is Send+Sync and construction is expensive.
    let cli_parser = Arc::new(DefaultCliParserFactory.create("dbt-core"));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                handle_connection(stream, Arc::clone(&cli_parser), Arc::clone(&feature_stack))
                    .await;
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    cli_parser: Arc<dbt_clap_core::CliParser>,
    feature_stack: Arc<FeatureStack>,
) {
    let request: DaemonRequest = match read_frame(&mut stream).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Client disconnected before sending a request.
            return;
        }
        Err(e) => {
            error!("failed to read request: {e}");
            return;
        }
    };

    let exit_code = dispatch(request, &cli_parser, feature_stack).await;

    if let Err(e) = write_frame(&mut stream, &DaemonResponse { exit_code }).await {
        error!("failed to write response: {e}");
    }
}

/// Execute one dbt command and return its exit code.
async fn dispatch(
    request: DaemonRequest,
    cli_parser: &dbt_clap_core::CliParser,
    feature_stack: Arc<FeatureStack>,
) -> i32 {
    // Temporarily change the working directory if the client supplied one.
    // This is safe because requests are serialised.
    let prev_cwd = request.cwd.as_ref().and_then(|cwd| {
        let prev = std::env::current_dir().ok();
        if let Err(e) = std::env::set_current_dir(cwd) {
            error!("failed to set cwd to {cwd}: {e}");
        }
        prev
    });

    let exit_code = run_command(request.args, cli_parser, feature_stack).await;

    // Restore the previous working directory.
    if let Some(prev) = prev_cwd {
        let _ = std::env::set_current_dir(prev);
    }

    exit_code
}

async fn run_command(
    args: Vec<String>,
    cli_parser: &dbt_clap_core::CliParser,
    feature_stack: Arc<FeatureStack>,
) -> i32 {
    let cli = match cli_parser.try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // Print the clap error the same way the normal CLI does.
            eprintln!("{e}");
            return 1;
        }
    };

    let system_arg = from_main(&cli);

    // A fresh never-cancelling token for each invocation.
    // Ctrl+C sent to the daemon process can be caught at a higher level if needed.
    let token = never_cancels();

    let result = execute_fs_and_shutdown(system_arg, cli, false, feature_stack, token).await;

    match result {
        Ok(()) => 0,
        Err(e) => e.exit_status().unwrap_or(1) as i32,
    }
}
