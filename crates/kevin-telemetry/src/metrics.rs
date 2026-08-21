//! Metrics (`plan/10-observability-ops.md` §Metrics).
//!
//! `metrics` facade + Prometheus exporter. Names are declared here as
//! constants (prefix `kevin_`), described once by [`describe_all`], and served
//! by [`serve_metrics`] on `telemetry.metrics_bind` (a separate listener,
//! never the API bind) or rendered through [`MetricsHandle::render`].
//!
//! Labels are **bounded enums only** — never ids, paths, prompts or error
//! messages. [`bounded`] clamps a free value to an allow-list.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use metrics::{Unit, describe_counter, describe_gauge, describe_histogram};
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

macro_rules! metric_names {
    ($( $(#[$m:meta])* $konst:ident = $name:literal ; )+) => {
        $( $(#[$m])* pub const $konst: &str = $name; )+
        /// Every metric name declared by Kevin.
        pub const ALL: &[&str] = &[$($konst),+];
    };
}

metric_names! {
    /// gauge(1) `version`, `commit`, `profile`.
    BUILD_INFO = "kevin_build_info";
    /// counter `mode`, `outcome`.
    RUNS_TOTAL = "kevin_runs_total";
    /// gauge `status`.
    RUNS_ACTIVE = "kevin_runs_active";
    /// histogram `mode`, `outcome`.
    RUN_DURATION_SECONDS = "kevin_run_duration_seconds";
    /// histogram `phase`.
    RUN_PHASE_DURATION_SECONDS = "kevin_run_phase_duration_seconds";
    /// counter `kind`, `outcome`.
    TASKS_TOTAL = "kevin_tasks_total";
    /// gauge `kind`, `status`.
    TASKS_ACTIVE = "kevin_tasks_active";
    /// counter `kind`, `worker`, `model_alias`, `outcome`.
    TASK_ATTEMPTS_TOTAL = "kevin_task_attempts_total";
    /// histogram `kind`, `worker`, `model_alias`.
    TASK_ATTEMPT_DURATION_SECONDS = "kevin_task_attempt_duration_seconds";
    /// counter `kind`, `failure_class`.
    TASK_RETRIES_TOTAL = "kevin_task_retries_total";
    /// counter `outcome`.
    QUESTIONS_TOTAL = "kevin_questions_total";
    /// histogram `mode`.
    QUESTION_WAIT_SECONDS = "kevin_question_wait_seconds";
    /// gauge `worker`.
    WORKER_PROCESSES = "kevin_worker_processes";
    /// counter `worker`, `class`.
    WORKER_EXITS_TOTAL = "kevin_worker_exits_total";
    /// histogram `worker`.
    WORKER_SPAWN_DURATION_SECONDS = "kevin_worker_spawn_duration_seconds";
    /// gauge `worker`.
    WORKER_SEMAPHORE_WAITERS = "kevin_worker_semaphore_waiters";
    /// counter `model_alias`, `direction`.
    TOKENS_TOTAL = "kevin_tokens_total";
    /// counter(float) `model_alias`, `role_or_kind`.
    COST_USD_TOTAL = "kevin_cost_usd_total";
    /// counter `dimension`.
    BUDGET_EXHAUSTED_TOTAL = "kevin_budget_exhausted_total";
    /// gauge.
    SCHEDULER_READY_TASKS = "kevin_scheduler_ready_tasks";
    /// gauge `reason`.
    SCHEDULER_BLOCKED_TASKS = "kevin_scheduler_blocked_tasks";
    /// histogram `aggregate_type`.
    EVENT_STORE_APPEND_DURATION_SECONDS = "kevin_event_store_append_duration_seconds";
    /// counter `aggregate_type`.
    EVENT_STORE_VERSION_CONFLICTS_TOTAL = "kevin_event_store_version_conflicts_total";
    /// counter `event_type`.
    EVENTS_APPENDED_TOTAL = "kevin_events_appended_total";
    /// gauge.
    OUTBOX_BACKLOG = "kevin_outbox_backlog";
    /// gauge.
    OUTBOX_OLDEST_AGE_SECONDS = "kevin_outbox_oldest_age_seconds";
    /// gauge `projection`.
    PROJECTION_LAG_EVENTS = "kevin_projection_lag_events";
    /// histogram `projection`.
    PROJECTION_APPLY_DURATION_SECONDS = "kevin_projection_apply_duration_seconds";
    /// counter `subscriber`.
    BUS_LAGGED_TOTAL = "kevin_bus_lagged_total";
    /// histogram `embedder`.
    MEMORY_SEARCH_DURATION_SECONDS = "kevin_memory_search_duration_seconds";
    /// gauge `kind`, `scope_type`.
    MEMORY_ITEMS = "kevin_memory_items";
    /// histogram `embedder`.
    EMBEDDING_DURATION_SECONDS = "kevin_embedding_duration_seconds";
    /// counter `kind`, `policy`, `model_alias`, `explored`.
    ROUTER_SELECTIONS_TOTAL = "kevin_router_selections_total";
    /// histogram `rubric`, `subject`.
    EVAL_OVERALL_SCORE = "kevin_eval_overall_score";
    /// counter `kind`, `status`.
    EVAL_PROPOSALS_TOTAL = "kevin_eval_proposals_total";
    /// counter `route`, `method`, `status_class`.
    API_REQUESTS_TOTAL = "kevin_api_requests_total";
    /// histogram `route`, `method`.
    API_REQUEST_DURATION_SECONDS = "kevin_api_request_duration_seconds";
    /// gauge.
    API_SSE_CONNECTIONS = "kevin_api_sse_connections";
    /// counter `outcome`.
    KOHRAL_TURNS_TOTAL = "kevin_kohral_turns_total";
    /// gauge.
    KOHRAL_TURNS_ACTIVE = "kevin_kohral_turns_active";
    /// gauge(0/1).
    KOHRAL_DRAINING = "kevin_kohral_draining";
    /// gauge `state`.
    DB_POOL_CONNECTIONS = "kevin_db_pool_connections";
    /// counter `level`.
    TELEMETRY_DROPPED_RECORDS_TOTAL = "kevin_telemetry_dropped_records_total";
}

/// Duration histogram buckets: `0.05..3600 s`, log-scaled.
pub const DURATION_BUCKETS: &[f64] = &[
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];
/// Score histogram buckets: `0.0..1.0` step `0.1`.
pub const SCORE_BUCKETS: &[f64] = &[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0];

/// Clamps a free-form label value to an allow-list; anything else becomes
/// `"other"`, so label cardinality stays bounded.
#[must_use]
pub fn bounded(value: &str, allowed: &'static [&'static str]) -> &'static str {
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or("other")
}

/// Registers descriptions and units for every metric in [`ALL`]. Idempotent.
pub fn describe_all() {
    describe_gauge!(BUILD_INFO, "Build information (always 1).");
    describe_counter!(RUNS_TOTAL, "Runs finished, by mode and outcome.");
    describe_gauge!(RUNS_ACTIVE, "Runs currently in a non-terminal status.");
    describe_histogram!(
        RUN_DURATION_SECONDS,
        Unit::Seconds,
        "Run wall-clock duration."
    );
    describe_histogram!(
        RUN_PHASE_DURATION_SECONDS,
        Unit::Seconds,
        "Run phase duration."
    );
    describe_counter!(TASKS_TOTAL, "Tasks finished, by kind and outcome.");
    describe_gauge!(TASKS_ACTIVE, "Tasks currently in a non-terminal status.");
    describe_counter!(
        TASK_ATTEMPTS_TOTAL,
        "Task attempts finished, by route and outcome."
    );
    describe_histogram!(
        TASK_ATTEMPT_DURATION_SECONDS,
        Unit::Seconds,
        "Task attempt duration."
    );
    describe_counter!(
        TASK_RETRIES_TOTAL,
        "Task retries, by kind and failure class."
    );
    describe_counter!(QUESTIONS_TOTAL, "Questions resolved, by outcome.");
    describe_histogram!(
        QUESTION_WAIT_SECONDS,
        Unit::Seconds,
        "Time a question waited for an answer."
    );
    describe_gauge!(WORKER_PROCESSES, "Running worker subprocesses.");
    describe_counter!(WORKER_EXITS_TOTAL, "Worker subprocess exits, by class.");
    describe_histogram!(
        WORKER_SPAWN_DURATION_SECONDS,
        Unit::Seconds,
        "Time to spawn a worker subprocess."
    );
    describe_gauge!(
        WORKER_SEMAPHORE_WAITERS,
        "Attempts waiting for a worker bulkhead permit."
    );
    describe_counter!(
        TOKENS_TOTAL,
        "Tokens consumed, by model alias and direction."
    );
    describe_counter!(COST_USD_TOTAL, "Estimated spend in USD.");
    describe_counter!(
        BUDGET_EXHAUSTED_TOTAL,
        "Budget exhaustion events, by dimension."
    );
    describe_gauge!(SCHEDULER_READY_TASKS, "Tasks ready to run.");
    describe_gauge!(SCHEDULER_BLOCKED_TASKS, "Tasks blocked, by reason.");
    describe_histogram!(
        EVENT_STORE_APPEND_DURATION_SECONDS,
        Unit::Seconds,
        "Event store append latency."
    );
    describe_counter!(
        EVENT_STORE_VERSION_CONFLICTS_TOTAL,
        "Optimistic concurrency conflicts."
    );
    describe_counter!(EVENTS_APPENDED_TOTAL, "Domain events appended, by type.");
    describe_gauge!(OUTBOX_BACKLOG, "Outbox rows not yet relayed.");
    describe_gauge!(
        OUTBOX_OLDEST_AGE_SECONDS,
        Unit::Seconds,
        "Age of the oldest unrelayed outbox row."
    );
    describe_gauge!(
        PROJECTION_LAG_EVENTS,
        "Events between the head and a projection checkpoint."
    );
    describe_histogram!(
        PROJECTION_APPLY_DURATION_SECONDS,
        Unit::Seconds,
        "Projection apply latency."
    );
    describe_counter!(
        BUS_LAGGED_TOTAL,
        "Bus subscribers that lagged behind the broadcast channel."
    );
    describe_histogram!(
        MEMORY_SEARCH_DURATION_SECONDS,
        Unit::Seconds,
        "Memory search latency."
    );
    describe_gauge!(MEMORY_ITEMS, "Memory items stored, by kind and scope type.");
    describe_histogram!(
        EMBEDDING_DURATION_SECONDS,
        Unit::Seconds,
        "Embedding latency."
    );
    describe_counter!(
        ROUTER_SELECTIONS_TOTAL,
        "Route selections, by kind, policy and alias."
    );
    describe_histogram!(EVAL_OVERALL_SCORE, "Evaluation overall score (0..1).");
    describe_counter!(
        EVAL_PROPOSALS_TOTAL,
        "Evaluation proposals, by kind and status."
    );
    describe_counter!(
        API_REQUESTS_TOTAL,
        "HTTP API requests, by route, method and status class."
    );
    describe_histogram!(
        API_REQUEST_DURATION_SECONDS,
        Unit::Seconds,
        "HTTP API request latency."
    );
    describe_gauge!(API_SSE_CONNECTIONS, "Open SSE connections.");
    describe_counter!(KOHRAL_TURNS_TOTAL, "Kohral turns finished, by outcome.");
    describe_gauge!(KOHRAL_TURNS_ACTIVE, "Kohral turns in flight.");
    describe_gauge!(KOHRAL_DRAINING, "1 while draining, else 0.");
    describe_gauge!(DB_POOL_CONNECTIONS, "Database pool connections, by state.");
    describe_counter!(
        TELEMETRY_DROPPED_RECORDS_TOTAL,
        "Log records dropped by the non-blocking writer, by level."
    );
}

/// Handle over the installed Prometheus recorder.
#[derive(Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsHandle").finish_non_exhaustive()
    }
}

impl MetricsHandle {
    /// Renders the registry in the Prometheus text exposition format (the body
    /// of `GET /metrics`). Runs registry upkeep first so histograms stay bounded.
    #[must_use]
    pub fn render(&self) -> String {
        self.inner.run_upkeep();
        self.inner.render()
    }
}

/// Installs the Prometheus recorder as the global `metrics` recorder (once per
/// process; later calls return the same handle) and registers descriptions.
pub fn install() -> Result<MetricsHandle, BuildError> {
    static HANDLE: OnceLock<MetricsHandle> = OnceLock::new();
    if let Some(handle) = HANDLE.get() {
        return Ok(handle.clone());
    }
    let recorder = PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), DURATION_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(EVAL_OVERALL_SCORE.into()), SCORE_BUCKETS)?
        .install_recorder()?;
    let handle = HANDLE.get_or_init(|| MetricsHandle { inner: recorder });
    describe_all();
    Ok(handle.clone())
}

/// Serves `GET /metrics` (and `/`) on `bind` from a dedicated listener.
///
/// Returns the bound address (useful with port 0) and the server task; abort
/// the task (or drop the [`crate::Guard`] holding it) to stop serving.
pub async fn serve_metrics(
    bind: SocketAddr,
    handle: MetricsHandle,
) -> std::io::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    let task = tokio::spawn(serve_on(listener, handle));
    Ok((local, task))
}

/// Serves the registry on an already-bound listener until aborted.
pub async fn serve_on(listener: TcpListener, handle: MetricsHandle) {
    {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let handle = handle.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                // Read (and ignore) the request head; bounded by a short timeout.
                let _ = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buf)).await;
                let body = handle.render();
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_prefixed_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for name in ALL {
            assert!(name.starts_with("kevin_"), "{name}");
            assert!(seen.insert(name), "duplicate {name}");
        }
        assert_eq!(ALL.len(), 43);
    }

    #[test]
    fn bounded_clamps_unknown_values() {
        const ALLOWED: &[&str] = &["ok", "timeout"];
        assert_eq!(bounded("ok", ALLOWED), "ok");
        assert_eq!(bounded("sk-secret", ALLOWED), "other");
    }
}
