//! The daemon's metric observers (`plan/10-observability-ops.md` §Metrics).
//!
//! The engine already instruments what only it can see — attempts, retries,
//! routing, bulkheads, store latency, projection lag. What is left are the
//! numbers that belong to the *process*, not to a saga, and this module owns
//! them. Two tasks, both started by [`spawn`] and stopped with the runtime's
//! cancellation token:
//!
//! - [`EventMetrics`] — a bus subscriber that turns committed events into the
//!   durations nobody else can measure: how long a run took end to end
//!   (`kevin_run_duration_seconds`), how long each phase took
//!   (`kevin_run_phase_duration_seconds`), how long a question waited
//!   (`kevin_question_wait_seconds`), and the money spent
//!   (`kevin_cost_usd_total`).
//! - [`sample`] — a periodic gauge sweep over the read models and the pool:
//!   `kevin_tasks_active`, `kevin_scheduler_blocked_tasks`,
//!   `kevin_outbox_backlog`, `kevin_outbox_oldest_age_seconds`,
//!   `kevin_memory_items`, `kevin_db_pool_connections`, `kevin_kohral_draining`.
//!
//! Deriving from events rather than from the saga keeps the labels bounded
//! (kinds, modes, outcomes — never ids) and means a metric can never change
//! what the runtime does.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kevin_bus::{BusMessage, EventBus, SubscriptionFilter};
use kevin_domain::question::QuestionEvent;
use kevin_domain::run::RunEvent;
use kevin_domain::task::TaskEvent;
use kevin_domain::values::{RunMode, Usage};
use kevin_orchestrator::Handle;
use kevin_store::PgPool;
use kevin_telemetry::metrics as names;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How often the gauge sweep runs.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Upper bound on remembered aggregates, so a very long-lived daemon with
/// abandoned streams cannot grow the fold's maps without limit.
const MAX_TRACKED: usize = 8192;

/// Statuses a task can hold while it is still alive. Every kind that has ever
/// appeared reports all four, so a status that emptied reads `0` instead of
/// vanishing from the exposition.
const ACTIVE_TASK_STATUSES: [&str; 4] = ["pending", "routed", "running", "awaiting_input"];

/// Registers every gauge whose "nothing has happened yet" value is meaningful,
/// so a scrape taken one second after startup already exposes it.
pub fn prime() {
    metrics::gauge!(names::OUTBOX_BACKLOG).set(0.0);
    metrics::gauge!(names::OUTBOX_OLDEST_AGE_SECONDS).set(0.0);
    for reason in ["deps", "semaphore", "budget"] {
        metrics::gauge!(names::SCHEDULER_BLOCKED_TASKS, "reason" => reason).set(0.0);
    }
    metrics::gauge!(names::KOHRAL_DRAINING).set(0.0);
    for state in ["idle", "in_use"] {
        metrics::gauge!(names::DB_POOL_CONNECTIONS, "state" => state).set(0.0);
    }
}

/// Starts the event subscriber and the gauge sweep; both stop on `cancel`.
#[must_use]
pub fn spawn(
    bus: &Arc<dyn EventBus>,
    pool: PgPool,
    handle: Arc<Handle>,
    cancel: &CancellationToken,
) -> Vec<JoinHandle<()>> {
    prime();
    let events = {
        let mut stream = bus.subscribe(SubscriptionFilter::all().named("metrics"));
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut folder = EventMetrics::default();
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    message = stream.next() => match message {
                        Some(BusMessage::Live(event)) => folder.observe(&event.envelope),
                        Some(BusMessage::Lagged { .. }) => {}
                        None => break,
                    },
                }
            }
        })
    };
    let gauges = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => sample(&pool, &handle).await,
                }
            }
        })
    };
    vec![events, gauges]
}

// ---------------------------------------------------------------------------
// Event-derived durations and spend
// ---------------------------------------------------------------------------

/// What a run's terminal event does not repeat but its histogram needs.
#[derive(Debug, Clone)]
struct RunFacts {
    mode: &'static str,
    started_at: DateTime<Utc>,
    phase_started_at: DateTime<Utc>,
    phase: &'static str,
}

/// Turns committed events into process-level durations and spend.
#[derive(Debug, Default)]
pub struct EventMetrics {
    runs: HashMap<Uuid, RunFacts>,
    questions: HashMap<Uuid, DateTime<Utc>>,
    aliases: HashMap<Uuid, String>,
}

impl EventMetrics {
    /// Records the metrics implied by one committed event.
    pub fn observe(&mut self, event: &kevin_bus::Event) {
        match event.aggregate_type {
            "run" => {
                if let Ok(payload) = serde_json::from_value::<RunEvent>(event.payload.clone()) {
                    self.run(event.aggregate_id, event.occurred_at, &payload);
                }
            }
            "task" => {
                if let Ok(payload) = serde_json::from_value::<TaskEvent>(event.payload.clone()) {
                    self.task(event.aggregate_id, &payload);
                }
            }
            "question" => {
                if let Ok(payload) = serde_json::from_value::<QuestionEvent>(event.payload.clone())
                {
                    self.question(event.aggregate_id, event.occurred_at, &payload);
                }
            }
            _ => {}
        }
    }

    fn run(&mut self, run_id: Uuid, at: DateTime<Utc>, event: &RunEvent) {
        match event {
            RunEvent::Started { mode, .. } => {
                if self.runs.len() >= MAX_TRACKED {
                    self.runs.clear();
                }
                self.runs.insert(
                    run_id,
                    RunFacts {
                        mode: mode_label(mode),
                        started_at: at,
                        phase_started_at: at,
                        phase: "intake",
                    },
                );
            }
            RunEvent::UnderstandingStarted { .. } => self.phase(run_id, at, "understanding"),
            RunEvent::PlanProposed { .. } => self.phase(run_id, at, "planning"),
            RunEvent::ExecutionStarted { .. } => self.phase(run_id, at, "executing"),
            RunEvent::Integrated { .. } => self.phase(run_id, at, "integrating"),
            RunEvent::Evaluated { .. } => self.phase(run_id, at, "evaluating"),
            RunEvent::Completed { .. } => self.run_finished(run_id, at, "completed"),
            RunEvent::Failed { .. } => self.run_finished(run_id, at, "failed"),
            RunEvent::Cancelled { .. } => self.run_finished(run_id, at, "cancelled"),
            _ => {}
        }
    }

    /// Closes the current phase's histogram sample and opens the next one.
    fn phase(&mut self, run_id: Uuid, at: DateTime<Utc>, next: &'static str) {
        let Some(facts) = self.runs.get_mut(&run_id) else {
            return;
        };
        metrics::histogram!(names::RUN_PHASE_DURATION_SECONDS, "phase" => facts.phase)
            .record(elapsed(facts.phase_started_at, at));
        facts.phase = next;
        facts.phase_started_at = at;
    }

    fn run_finished(&mut self, run_id: Uuid, at: DateTime<Utc>, outcome: &'static str) {
        let Some(facts) = self.runs.remove(&run_id) else {
            return;
        };
        metrics::histogram!(names::RUN_PHASE_DURATION_SECONDS, "phase" => facts.phase)
            .record(elapsed(facts.phase_started_at, at));
        metrics::histogram!(
            names::RUN_DURATION_SECONDS,
            "mode" => facts.mode,
            "outcome" => outcome,
        )
        .record(elapsed(facts.started_at, at));
    }

    /// Spend, labelled with the alias the task was routed to.
    fn task(&mut self, task_id: Uuid, event: &TaskEvent) {
        match event {
            TaskEvent::Routed { route, .. } => {
                if self.aliases.len() >= MAX_TRACKED {
                    self.aliases.clear();
                }
                self.aliases.insert(task_id, route.model.to_string());
            }
            TaskEvent::AttemptSucceeded { usage, .. } | TaskEvent::AttemptFailed { usage, .. } => {
                self.cost(task_id, usage);
            }
            TaskEvent::Progressed { usage_delta, .. } => self.cost(task_id, usage_delta),
            _ => {}
        }
    }

    fn cost(&self, task_id: Uuid, usage: &Usage) {
        let Some(cost) = usage.cost_usd else { return };
        let alias = self
            .aliases
            .get(&task_id)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());
        // `plan/10` types this as a float counter; the `metrics` facade only
        // has u64 counters, so it is a monotonically incremented gauge —
        // `rate()` over it still reads correctly, and `orch.cost_ledger`
        // stays the authoritative ledger for `kevin cost`.
        metrics::gauge!(
            names::COST_USD_TOTAL,
            "model_alias" => alias,
            "role_or_kind" => "task",
        )
        .increment(decimal(cost));
    }

    fn question(&mut self, question_id: Uuid, at: DateTime<Utc>, event: &QuestionEvent) {
        match event {
            QuestionEvent::Asked { .. } => {
                if self.questions.len() >= MAX_TRACKED {
                    self.questions.clear();
                }
                self.questions.insert(question_id, at);
            }
            QuestionEvent::Answered { answered_by, .. } => {
                if let Some(asked_at) = self.questions.remove(&question_id) {
                    let mode = if answered_by == "default" {
                        "headless"
                    } else {
                        "interactive"
                    };
                    metrics::histogram!(names::QUESTION_WAIT_SECONDS, "mode" => mode)
                        .record(elapsed(asked_at, at));
                }
            }
            QuestionEvent::Expired { .. } => {
                self.questions.remove(&question_id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Gauge sweep
// ---------------------------------------------------------------------------

/// One pass of the gauge sweep. Every query is a cheap aggregate; a failing
/// query is skipped — metrics never take the process down.
pub async fn sample(pool: &PgPool, handle: &Arc<Handle>) {
    pool_gauges(pool);
    metrics::gauge!(names::KOHRAL_DRAINING).set(f64::from(u8::from(!handle.is_admitting())));

    task_gauges(pool).await;

    blocked_gauges(pool).await;
    outbox_gauges(pool).await;

    if let Ok(rows) = grouped_pairs(
        pool,
        "SELECT kind, CASE WHEN scope = 'global' THEN 'global' ELSE 'repo' END, count(*) \
         FROM memory.memory_items WHERE forgotten_at IS NULL GROUP BY 1, 2",
    )
    .await
    {
        for (kind, scope, count) in rows {
            metrics::gauge!(names::MEMORY_ITEMS, "kind" => kind, "scope_type" => scope)
                .set(rows_to_f64(count));
        }
    }
}

/// `kevin_tasks_active{kind,status}` for every kind on the board.
async fn task_gauges(pool: &PgPool) {
    let Ok(rows) = grouped_pairs(
        pool,
        "SELECT kind, status, count(*) FROM orch.task_board GROUP BY kind, status",
    )
    .await
    else {
        return;
    };
    let mut kinds: std::collections::BTreeMap<String, [i64; 4]> = std::collections::BTreeMap::new();
    for (kind, status, count) in rows {
        let slot = kinds.entry(kind).or_default();
        if let Some(index) = ACTIVE_TASK_STATUSES.iter().position(|s| *s == status) {
            slot[index] = count;
        }
    }
    for (kind, counts) in kinds {
        for (status, count) in ACTIVE_TASK_STATUSES.iter().zip(counts) {
            metrics::gauge!(names::TASKS_ACTIVE, "kind" => kind.clone(), "status" => *status)
                .set(rows_to_f64(count));
        }
    }
}

fn pool_gauges(pool: &PgPool) {
    let idle = rows_to_f64(i64::try_from(pool.num_idle()).unwrap_or(i64::MAX));
    let total = f64::from(pool.size());
    metrics::gauge!(names::DB_POOL_CONNECTIONS, "state" => "idle").set(idle);
    metrics::gauge!(names::DB_POOL_CONNECTIONS, "state" => "in_use").set((total - idle).max(0.0));
}

/// `pending` waits on a dependency, `routed` has a route and waits only for a
/// bulkhead permit — the two reasons plan/10 asks this gauge to separate.
async fn blocked_gauges(pool: &PgPool) {
    let counts = sqlx::query_as::<_, (i64, i64)>(
        "SELECT count(*) FILTER (WHERE status = 'routed'), \
                count(*) FILTER (WHERE status = 'pending') \
         FROM orch.task_board",
    )
    .fetch_one(pool)
    .await;
    let Ok((routed, pending)) = counts else {
        return;
    };
    metrics::gauge!(names::SCHEDULER_BLOCKED_TASKS, "reason" => "deps").set(rows_to_f64(pending));
    metrics::gauge!(names::SCHEDULER_BLOCKED_TASKS, "reason" => "semaphore")
        .set(rows_to_f64(routed));
}

async fn outbox_gauges(pool: &PgPool) {
    let row = sqlx::query_as::<_, (i64, Option<f64>)>(
        "SELECT count(*), max(extract(epoch FROM now() - created_at))::float8 \
         FROM core.outbox WHERE delivered_at IS NULL",
    )
    .fetch_one(pool)
    .await;
    if let Ok((backlog, oldest)) = row {
        metrics::gauge!(names::OUTBOX_BACKLOG).set(rows_to_f64(backlog));
        metrics::gauge!(names::OUTBOX_OLDEST_AGE_SECONDS).set(oldest.unwrap_or(0.0));
    }
}

async fn grouped_pairs(
    pool: &PgPool,
    sql: &str,
) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, i64)>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(pool)
        .await
}

/// Seconds between two event timestamps, never negative.
fn elapsed(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    rows_to_f64((to - from).num_milliseconds().max(0)) / 1000.0
}

/// A row count or a millisecond span as a gauge value.
#[allow(clippy::cast_precision_loss, reason = "counts never exceed 2^53")]
const fn rows_to_f64(value: i64) -> f64 {
    value as f64
}

fn decimal(value: rust_decimal::Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive as _;
    value.to_f64().unwrap_or(0.0)
}

const fn mode_label(mode: &RunMode) -> &'static str {
    match mode {
        RunMode::Interactive => "interactive",
        RunMode::Headless => "headless",
        RunMode::Kohral { .. } => "kohral",
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use kevin_domain::values::RunMode;

    use super::{decimal, elapsed, mode_label};

    #[test]
    fn scalars_convert_the_way_prometheus_expects() {
        let a = Utc.timestamp_opt(1_700_000_000, 0).single().expect("ts");
        let b = a + chrono::Duration::milliseconds(1_500);
        assert!((elapsed(a, b) - 1.5).abs() < f64::EPSILON);
        assert!(elapsed(b, a).abs() < f64::EPSILON, "never negative");
        assert!((decimal(rust_decimal::Decimal::new(125, 2)) - 1.25).abs() < f64::EPSILON);
    }

    #[test]
    fn modes_are_bounded_labels() {
        assert_eq!(mode_label(&RunMode::Interactive), "interactive");
        assert_eq!(mode_label(&RunMode::Headless), "headless");
        assert_eq!(
            mode_label(&RunMode::Kohral {
                turn_id: "t".to_owned(),
                session_key: "s".to_owned(),
                session_id: "i".to_owned(),
            }),
            "kohral"
        );
    }
}
