//! Unix socket server: accepts connections and dispatches dbt commands.
//!
//! Requests are processed **serially** — one at a time.  This mirrors the way
//! dbt is normally used and avoids races on global process state (rustls
//! provider installation, metrics counters, cwd).
//!
//! # Caching
//!
//! Project parse state, the schema store, and the compiled-SQL cache are kept
//! alive in a [`CachedWorkerState`] between requests.  On each project command
//! the daemon calls [`DbtProjectCompilation::initialize_server`] with the
//! previous compilation so the incremental parser only re-reads changed files,
//! then calls [`DbtProjectCompilation::run_tasks`] with the previous cache
//! state so warehouse schema information is reused.  Non-project commands
//! (deps, clean, login, …) bypass the cache and use the stateless
//! `execute_fs_and_shutdown` path unchanged.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use dbt_clap_core::{CliParserFactory as _, Command, CoreCommand, from_main};
use dbt_common::{
    cancellation::never_cancels,
    constants::DBT_MANIFEST_JSON,
    io_args::FsCommand,
    tracing::dbt_emit::emit_error_log_from_fs_error,
};
use dbt_features::{cli::DefaultCliParserFactory, feature_stack::FeatureStack};
use dbt_jinja_utils::listener::{
    DefaultJinjaTypeCheckEventListenerFactory, JinjaTypeCheckingEventListenerFactory,
};
use dbt_lib::{
    compilation::{DbtProjectCompilation, DbtScheduleDescription},
    dbt_lib::execute_fs_and_shutdown,
};
use dbt_tasks_core::utils::write_run_results_json_or_warn;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::cache::CachedWorkerState;
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

    // Shared cache: populated on the first project command, reused thereafter.
    let cache: Arc<RwLock<Option<CachedWorkerState>>> = Arc::new(RwLock::new(None));

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                handle_connection(
                    stream,
                    Arc::clone(&cli_parser),
                    Arc::clone(&feature_stack),
                    Arc::clone(&cache),
                )
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
    cache: Arc<RwLock<Option<CachedWorkerState>>>,
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

    let exit_code = dispatch(request, &cli_parser, feature_stack, cache).await;

    if let Err(e) = write_frame(&mut stream, &DaemonResponse { exit_code }).await {
        error!("failed to write response: {e}");
    }
}

/// Execute one dbt command and return its exit code.
async fn dispatch(
    request: DaemonRequest,
    cli_parser: &dbt_clap_core::CliParser,
    feature_stack: Arc<FeatureStack>,
    cache: Arc<RwLock<Option<CachedWorkerState>>>,
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

    let exit_code = execute_server_command(request.args, cli_parser, feature_stack, cache).await;

    // Restore the previous working directory.
    if let Some(prev) = prev_cwd {
        let _ = std::env::set_current_dir(prev);
    }

    exit_code
}

/// Returns `true` for commands that require a loaded dbt project (compile, run,
/// test, build, …).  Infrastructure commands (deps, clean, login, …) return
/// `false` and are routed to the stateless `execute_fs_and_shutdown` path.
fn is_project_command(cli: &dbt_clap_core::Cli) -> bool {
    !matches!(
        &cli.command,
        Command::Core(
            CoreCommand::System(_)
                | CoreCommand::Man(_)
                | CoreCommand::Login(_)
                | CoreCommand::Docs(_)
                | CoreCommand::Init(_)
                | CoreCommand::Deps(_)
                | CoreCommand::Clean(_)
                | CoreCommand::Completions(_)
        )
    )
}

/// Execute one dbt command, reusing cached project state when available.
///
/// ## Cache warm-up (first project command)
///
/// On the first project command the cache is empty, so
/// `DbtProjectCompilation::initialize_server` is called with
/// `prev_compilation = None`.  This performs a full project parse, and the
/// result is stored in the cache.
///
/// ## Subsequent project commands
///
/// `initialize_server` is called with `prev_compilation = Some(cached)`.  The
/// incremental parser inside skips re-reading files whose on-disk mtime has not
/// changed, making each successive invocation significantly faster.
/// `run_tasks` receives the cached `DbtProjectCompilationCacheState` so the
/// `SchemaStore` and `CompiledSqlCache` are reused across runs.
async fn execute_server_command(
    args: Vec<String>,
    cli_parser: &dbt_clap_core::CliParser,
    feature_stack: Arc<FeatureStack>,
    cache: Arc<RwLock<Option<CachedWorkerState>>>,
) -> i32 {
    let cli = match cli_parser.try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            // Print the clap error the same way the normal CLI does.
            eprintln!("{e}");
            return 1;
        }
    };

    // Non-project commands bypass the cache entirely and run the full
    // stateless execution path (which handles telemetry, invocation
    // metadata, etc.).
    if !is_project_command(&cli) {
        let system_arg = from_main(&cli);
        let token = never_cancels();
        return match execute_fs_and_shutdown(system_arg, cli, false, feature_stack, token).await {
            Ok(()) => 0,
            Err(e) => e.exit_status().unwrap_or(1) as i32,
        };
    }

    let system_arg = from_main(&cli);
    let eval_arg = match cli.to_eval_args(system_arg) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };

    let token = never_cancels();
    let jinja_factory: Arc<dyn JinjaTypeCheckingEventListenerFactory> =
        Arc::new(DefaultJinjaTypeCheckEventListenerFactory::default());

    // Read the previous compilation and cache state (cheap — only Arc clones).
    let (prev_compilation, prev_cache_state) = {
        let guard = cache.read().await;
        match guard.as_ref() {
            Some(c) => (
                Some(Arc::clone(&c.compilation)),
                Some(Arc::clone(&c.cache_state)),
            ),
            None => (None, None),
        }
    };

    let start = SystemTime::now();

    // ── Phase 1 + 2: parse and resolve ──────────────────────────────────────
    //
    // When prev_compilation is Some, the loader compares file mtimes and only
    // re-reads nodes whose source files have changed — unchanged nodes are
    // taken straight from the previous compilation in memory.
    let (mut compilation, jinja_env, cache_changes) =
        match DbtProjectCompilation::initialize_server(
            &feature_stack,
            &eval_arg,
            &cli,
            Arc::clone(&jinja_factory),
            prev_compilation,
            &token,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                emit_error_log_from_fs_error(e.as_ref(), eval_arg.io.status_reporter.as_ref());
                return e.exit_status().unwrap_or(1) as i32;
            }
        };

    // ── Phase 3: build execution schedule ───────────────────────────────────
    let schedule = match compilation
        .create_schedule(
            &cli,
            &eval_arg,
            DbtScheduleDescription::Default,
            Default::default(),
            &token,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            emit_error_log_from_fs_error(e.as_ref(), eval_arg.io.status_reporter.as_ref());
            return e.exit_status().unwrap_or(1) as i32;
        }
    };

    // ── Phase 4 + 5: compile and run tasks ──────────────────────────────────
    //
    // Passing prev_cache_state carries the SchemaStore, DataStore, and
    // CompiledSqlCache from the previous invocation so warehouse round-trips
    // and SQL re-compilation are minimised.
    let hooks_factory = Arc::clone(&feature_stack.task_runner.hooks_factory);
    let run_result = compilation
        .run_tasks(
            &eval_arg,
            &cli,
            start,
            jinja_env,
            Arc::clone(&feature_stack),
            schedule,
            cache_changes.as_ref(),
            prev_cache_state,
            Arc::clone(&jinja_factory),
            hooks_factory.as_ref(),
            &token,
        )
        .await;

    let (_run_task_args, run_task_results, new_jinja_env, _adapter, new_cache_state) =
        match run_result {
            Ok(r) => r,
            Err(e) => {
                // Write the manifest even on error so downstream tools (LSP,
                // codex ingestion) stay in sync with the parsed project state.
                if eval_arg.write_json {
                    write_manifest_json(&compilation.take_dbt_manifest(), &eval_arg.io.out_dir);
                }
                emit_error_log_from_fs_error(e.as_ref(), eval_arg.io.status_reporter.as_ref());
                return e.exit_status().unwrap_or(1) as i32;
            }
        };

    // Write JSON artifacts, matching standard dbt CLI output.
    if eval_arg.write_json {
        if eval_arg.command != FsCommand::Parse {
            write_run_results_json_or_warn(&run_task_results.stats.run, &eval_arg);
        }
        write_manifest_json(&compilation.take_dbt_manifest(), &eval_arg.io.out_dir);
    }

    // ── Update cache ─────────────────────────────────────────────────────────
    //
    // Store the fresh compilation and cache state.  The next request will pick
    // these up and call initialize_server(prev=Some(...)), enabling an
    // incremental parse that skips unchanged files.
    *cache.write().await = Some(CachedWorkerState {
        compilation: Arc::new(compilation),
        cache_state: new_cache_state,
        jinja_env: new_jinja_env,
    });

    0
}

/// Serialise `manifest` to `<out_dir>/manifest.json` using `serde_json`
/// directly.
///
/// This deliberately avoids `dbt_common::artifact_io::write_artifact_to_file`
/// (which emits a telemetry span and requires the `dbt-telemetry` crate) to
/// keep the daemon's dependency footprint minimal.
fn write_manifest_json<T: serde::Serialize>(manifest: &T, out_dir: &std::path::Path) {
    let path = out_dir.join(DBT_MANIFEST_JSON);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::File::create(&path) {
        Ok(f) => {
            let mut w = std::io::BufWriter::new(f);
            if let Err(e) = serde_json::to_writer(&mut w, manifest) {
                error!("failed to write manifest.json: {e}");
            } else if let Err(e) = w.flush() {
                error!("failed to flush manifest.json: {e}");
            }
        }
        Err(e) => error!("failed to create manifest.json at {}: {e}", path.display()),
    }
}
