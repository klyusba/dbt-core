//! Cached project state shared across consecutive daemon requests.

use std::sync::Arc;

use dbt_jinja_utils::jinja_environment::JinjaEnv;
use dbt_lib::compilation::{DbtProjectCompilation, DbtProjectCompilationCacheState};

/// Project state kept alive between daemon requests to avoid re-parsing the
/// project on every invocation.
///
/// An instance is stored behind `Arc<tokio::sync::RwLock<Option<CachedWorkerState>>>`
/// in the server so that each completed request can atomically replace the
/// cached state before the next request is accepted.
pub struct CachedWorkerState {
    /// Resolved project compilation from the previous invocation.
    ///
    /// Passed as `prev_compilation` to
    /// `DbtProjectCompilation::initialize_server` so the incremental parser
    /// can skip re-reading files that have not changed on disk.
    pub compilation: Arc<DbtProjectCompilation>,

    /// Schema store, data store, and compiled-SQL cache from the previous
    /// `run_tasks` call.
    ///
    /// Passed as `previous_cache_state` to `run_tasks` so warehouse schema
    /// information and compiled SQL survive across invocations.
    pub cache_state: Arc<DbtProjectCompilationCacheState>,

    /// Jinja environment returned by the last `run_tasks` call.
    pub jinja_env: Arc<JinjaEnv>,
}
