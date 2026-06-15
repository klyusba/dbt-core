//! `dbt-daemon` — persistent dbt execution daemon.
//!
//! # Modes
//!
//! ## Server mode  (`dbt-daemon serve [--socket PATH]`)
//!
//! Starts a long-running process that keeps the project parse state, schema
//! cache, and adapter connection pool alive between invocations.  Listens on a
//! Unix domain socket for commands from clients.
//!
//! Output (logs, progress, errors) is written to the daemon's own stdout/stderr,
//! exactly as if you had run `dbt` directly.
//!
//! ## Client mode  (`dbt-daemon <dbt-args…>`)
//!
//! Any first argument other than `serve`, `stop`, or `status` is treated as a
//! dbt command to forward to a running daemon.  The client exits with the same
//! exit code the daemon returns.
//!
//! # Quick start
//!
//! ```sh
//! # Start the daemon in the background (output goes to daemon.log)
//! dbt-daemon serve --socket ~/.dbt/daemon.sock > daemon.log 2>&1 &
//!
//! # Forward commands (use the same socket)
//! export DBT_DAEMON_SOCKET=~/.dbt/daemon.sock
//! dbt-daemon run --select my_model --project-dir /path/to/project
//! dbt-daemon compile --select my_model --project-dir /path/to/project
//!
//! # Or set up a shell alias for transparency
//! alias dbt='dbt-daemon'
//! ```

mod cache;
mod client;
mod protocol;
mod server;
mod state;

use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use dbt_common::tracing::{FsTraceConfig, dbt_init::init_tracing};
use dbt_features::feature_stack::FeatureStack;
use dbt_features::feature_stack_builder::FeatureStackBuilder;
use dbt_features::tracing::TracingFeature;
use state::default_socket_path;
use tracing::info;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Dispatch based on the first argument.
    match args.get(1).map(|s| s.as_str()) {
        Some("serve") => {
            let socket_path = extract_socket_arg(&args).unwrap_or_else(default_socket_path);
            run_server(socket_path);
        }
        Some("stop") => {
            eprintln!("stop: send SIGTERM to the daemon process (use `kill $(cat ~/.dbt/daemon.pid)`).");
            process::exit(0);
        }
        Some("status") => {
            let socket_path = extract_socket_arg(&args).unwrap_or_else(default_socket_path);
            run_status(&socket_path);
        }
        Some("--help") | Some("-h") => {
            print_help();
            process::exit(0);
        }
        _ => {
            // Client mode: forward everything to the daemon.
            let socket_path = extract_socket_arg(&args).unwrap_or_else(default_socket_path);
            run_client(args, socket_path);
        }
    }
}

// ─── server mode ─────────────────────────────────────────────────────────────

fn run_server(socket_path: PathBuf) {
    // Initialise the global tracing subscriber once for the lifetime of the daemon.
    let tracing_feature = match init_tracing(FsTraceConfig::default()) {
        Ok((handle, provider)) => TracingFeature::default()
            .with_config_provider(provider)
            .with_shutdown_handle(handle),
        Err(e) => {
            eprintln!("dbt-daemon: failed to initialize tracing: {e}");
            process::exit(1);
        }
    };

    let feature_stack: Arc<FeatureStack> = FeatureStackBuilder::new(tracing_feature)
        .send_anonymous_usage_stats(false)
        .build()
        .into();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .max_blocking_threads(512)
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async move {
        info!("dbt-daemon starting (socket={})", socket_path.display());
        server::run_server(socket_path, feature_stack).await;
    });
}

// ─── status mode ─────────────────────────────────────────────────────────────

fn run_status(socket_path: &PathBuf) {
    if socket_path.exists() {
        println!("dbt-daemon: socket exists at {}", socket_path.display());
        println!("  (send a ping or check with `lsof -U {}` to confirm it is alive)", socket_path.display());
    } else {
        println!("dbt-daemon: no socket found at {}", socket_path.display());
        println!("  Start the daemon with: dbt-daemon serve");
        process::exit(1);
    }
}

// ─── client mode ─────────────────────────────────────────────────────────────

fn run_client(args: Vec<String>, socket_path: PathBuf) {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let exit_code = rt.block_on(async move {
        match client::send_command(&socket_path, args, cwd).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!(
                    "dbt-daemon: cannot connect to daemon at {} — {e}",
                    socket_path.display()
                );
                eprintln!("  Start the daemon with: dbt-daemon serve");
                2
            }
        }
    });

    process::exit(exit_code);
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Look for `--socket <PATH>` anywhere in `args` and return the path if found.
fn extract_socket_arg(args: &[String]) -> Option<PathBuf> {
    args.windows(2).find_map(|pair| {
        if pair[0] == "--socket" {
            Some(PathBuf::from(&pair[1]))
        } else {
            None
        }
    })
}

fn print_help() {
    println!("\
dbt-daemon — persistent dbt execution daemon

USAGE:
    dbt-daemon serve [--socket PATH]        Start the daemon server
    dbt-daemon status [--socket PATH]       Check whether daemon is running
    dbt-daemon stop                         How to stop the daemon
    dbt-daemon <dbt-args>                   Forward a dbt command to the daemon

ENVIRONMENT:
    DBT_DAEMON_SOCKET    Override the default socket path (~/.dbt/daemon.sock)

EXAMPLES:
    # Start daemon in the background
    dbt-daemon serve > /tmp/dbt-daemon.log 2>&1 &

    # Forward commands
    dbt-daemon run --select my_model --project-dir /path/to/project

    # Transparent alias
    alias dbt='dbt-daemon'
    dbt run --select my_model
");
}
