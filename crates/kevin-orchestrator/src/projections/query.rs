//! [`ReadModels`] — the typed query API over `orch.*` used by `kevin-api` and
//! `kevin-cli` (`plan/07-api-and-tui.md` §Endpoints). It never reads
//! `core.events`: interfaces query projections only.
//!
//! Money lives in `NUMERIC` columns and is always selected as text
//! ([`Usd`]), so decimals survive the round trip without a `NUMERIC` decoder.
//! Lists use keyset (cursor) pagination: `(updated_at, id)` for the boards,
//! `seq` for the task log.

use chrono::{DateTime, TimeZone as _, Utc};
use kevin_domain::Decimal;
use kevin_store::PgPool;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::postgres::{PgTypeInfo, PgValueRef};
use sqlx::{AssertSqlSafe, FromRow, Postgres, Row as _, ValueRef as _};
use uuid::Uuid;

use super::{ProjectionError, Result};

/// Default page size when a query does not set one.
pub const DEFAULT_LIMIT: usize = 50;
/// Largest page a query can ask for.
pub const MAX_LIMIT: usize = 500;

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

/// A USD amount read from a `numeric::text` column (`None` = unknown, which is
/// not the same as zero).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Usd(pub Option<Decimal>);

impl Usd {
    /// The amount, when known.
    #[must_use]
    pub const fn get(self) -> Option<Decimal> {
        self.0
    }
}

impl From<Usd> for Option<Decimal> {
    fn from(usd: Usd) -> Self {
        usd.0
    }
}

impl sqlx::Type<Postgres> for Usd {
    fn type_info() -> PgTypeInfo {
        <String as sqlx::Type<Postgres>>::type_info()
    }

    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as sqlx::Type<Postgres>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, Postgres> for Usd {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        if value.is_null() {
            return Ok(Usd(None));
        }
        let text = <String as sqlx::Decode<'_, Postgres>>::decode(value)?;
        Ok(Usd(Some(text.parse::<Decimal>()?)))
    }
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// Keyset cursor: "everything strictly older than this `(at, id)` pair".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Timestamp of the last item of the previous page.
    pub at: DateTime<Utc>,
    /// Id of the last item of the previous page (tie-breaker).
    pub id: Uuid,
}

impl Cursor {
    /// Builds a cursor.
    #[must_use]
    pub const fn new(at: DateTime<Utc>, id: Uuid) -> Self {
        Self { at, id }
    }

    /// Opaque string form (`<micros>.<uuid>`).
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{}.{}", self.at.timestamp_micros(), self.id)
    }

    /// Parses [`Cursor::encode`].
    pub fn decode(cursor: &str) -> Result<Self> {
        let invalid = || ProjectionError::InvalidCursor {
            cursor: cursor.to_owned(),
        };
        let (micros, id) = cursor.split_once('.').ok_or_else(invalid)?;
        let micros: i64 = micros.parse().map_err(|_| invalid())?;
        let at = Utc.timestamp_micros(micros).single().ok_or_else(invalid)?;
        let id = Uuid::parse_str(id).map_err(|_| invalid())?;
        Ok(Self { at, id })
    }
}

/// One page of rows plus the cursor for the next one (`Page<T>` in plan/07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The rows.
    pub items: Vec<T>,
    /// Cursor to pass to the next call; `None` when the page is the last one.
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// An empty page.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
        }
    }

    /// Number of rows in this page.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the page has no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn clamp_limit(limit: Option<usize>) -> i64 {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    i64::try_from(limit).unwrap_or(DEFAULT_LIMIT_I64)
}

/// [`DEFAULT_LIMIT`] as the `i64` Postgres wants.
const DEFAULT_LIMIT_I64: i64 = 50;

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// A row of `orch.run_overview` (`RunDto`/`RunSummaryDto`).
#[derive(Debug, Clone, FromRow)]
pub struct RunOverviewRow {
    /// Run id.
    pub run_id: Uuid,
    /// Run aggregate version this row reflects.
    pub version: i64,
    /// `RunStatus` as `snake_case`.
    pub status: String,
    /// Full goal text.
    pub goal_text: String,
    /// First line of the goal, for lists.
    pub goal_excerpt: String,
    /// Working directory.
    pub cwd: String,
    /// `git` | `jj` | `none`.
    pub repo_kind: String,
    /// `interactive` | `headless` | `kohral`.
    pub mode: String,
    /// Kohral turn/session ids for `kohral` runs.
    pub mode_detail: Option<Value>,
    /// Who asked.
    pub requested_by: String,
    /// Whether plans auto-approve.
    pub auto_approve_plans: bool,
    /// `Budget` as JSON.
    pub budget: Value,
    /// `Usage` as JSON.
    pub usage: Value,
    /// Total spend when known.
    pub cost_usd: Usd,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cache reads.
    pub cache_read_tokens: i64,
    /// Cache writes.
    pub cache_write_tokens: i64,
    /// Wall-clock milliseconds.
    pub wall_ms: i64,
    /// Route of the planner call.
    pub planner_route: Option<Value>,
    /// `Understanding` as JSON.
    pub understanding: Option<Value>,
    /// `Plan` as JSON.
    pub plan: Option<Value>,
    /// Plan revisions so far.
    pub plan_revision: i32,
    /// Questions still open.
    pub open_question_ids: Vec<Uuid>,
    /// Tasks of the approved plan.
    pub task_ids: Vec<Uuid>,
    /// Number of tasks.
    pub tasks_total: i32,
    /// Tasks that succeeded.
    pub tasks_succeeded: i32,
    /// Tasks that failed.
    pub tasks_failed: i32,
    /// Tasks that were cancelled.
    pub tasks_cancelled: i32,
    /// Tasks that were skipped.
    pub tasks_skipped: i32,
    /// Evaluation of the run, when done.
    pub evaluation_id: Option<Uuid>,
    /// Overall score 0..1.
    pub evaluation_overall: Option<f32>,
    /// Verdict.
    pub evaluation_verdict: Option<String>,
    /// Exhausted budget dimension, when any.
    pub budget_exhausted: Option<String>,
    /// Failure reason.
    pub failure_reason: Option<String>,
    /// Failure class.
    pub failure_class: Option<String>,
    /// Failure detail.
    pub failure_message: Option<String>,
    /// Who cancelled.
    pub cancelled_by: Option<String>,
    /// Why it was cancelled.
    pub cancel_reason: Option<String>,
    /// Final or integration summary.
    pub summary: Option<String>,
    /// Integration artifacts as JSON.
    pub artifacts: Value,
    /// `run.started` time.
    pub created_at: DateTime<Utc>,
    /// Last applied event time.
    pub updated_at: DateTime<Utc>,
    /// Global position of the last applied event.
    pub last_position: i64,
}

impl RunOverviewRow {
    /// Cursor pointing at this row.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.updated_at, self.run_id)
    }
}

/// A row of `orch.task_board` (`TaskDto`).
#[derive(Debug, Clone, FromRow)]
pub struct TaskBoardRow {
    /// Task id.
    pub task_id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Task aggregate version this row reflects.
    pub version: i64,
    /// Creation order within the run.
    pub seq: i32,
    /// `TaskKind` string form.
    pub kind: String,
    /// Spec title.
    pub title: String,
    /// Spec instructions.
    pub instructions: String,
    /// `TaskStatus` as `snake_case`.
    pub status: String,
    /// `TaskSpec` as JSON.
    pub spec: Value,
    /// Acceptance criteria as a JSON array.
    pub acceptance_criteria: Value,
    /// Task ids this task depends on.
    pub depends_on: Vec<Uuid>,
    /// `Budget` as JSON.
    pub budget: Value,
    /// Current `Route` as JSON.
    pub route: Option<Value>,
    /// Worker of the current route.
    pub route_worker: Option<String>,
    /// Model alias of the current route.
    pub route_model: Option<String>,
    /// Effort of the current route.
    pub route_effort: Option<String>,
    /// Router selection detail as JSON.
    pub selection: Option<Value>,
    /// `[AttemptDto]` as JSON, in attempt order.
    pub attempts: Value,
    /// Number of attempts started.
    pub attempt_count: i32,
    /// `Usage` as JSON (sum over attempts).
    pub usage: Value,
    /// Spend when known.
    pub cost_usd: Usd,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cache reads.
    pub cache_read_tokens: i64,
    /// Cache writes.
    pub cache_write_tokens: i64,
    /// Wall-clock milliseconds.
    pub wall_ms: i64,
    /// Artifacts produced, as JSON.
    pub artifacts: Value,
    /// Last summary from the worker.
    pub summary: Option<String>,
    /// Failure class of the last failure.
    pub failure_class: Option<String>,
    /// Failure message of the last failure.
    pub failure_message: Option<String>,
    /// Question the task is waiting on.
    pub awaiting_question_id: Option<Uuid>,
    /// First attempt start.
    pub started_at: Option<DateTime<Utc>>,
    /// Terminal time.
    pub ended_at: Option<DateTime<Utc>>,
    /// `task.created` time.
    pub created_at: DateTime<Utc>,
    /// Last applied event time.
    pub updated_at: DateTime<Utc>,
    /// Global position of the last applied event.
    pub last_position: i64,
}

impl TaskBoardRow {
    /// Cursor pointing at this row.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.updated_at, self.task_id)
    }
}

/// A row of `orch.question_inbox` (`QuestionDto`).
#[derive(Debug, Clone, FromRow)]
pub struct QuestionInboxRow {
    /// Question id.
    pub question_id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Asking task, when any.
    pub task_id: Option<Uuid>,
    /// Question aggregate version this row reflects.
    pub version: i64,
    /// The question.
    pub text: String,
    /// `[QuestionOption]` as JSON.
    pub options: Value,
    /// Whether several options may be selected.
    pub multi_select: bool,
    /// Default `Answer` as JSON.
    pub default_answer: Option<Value>,
    /// `QuestionPolicy` as JSON.
    pub policy: Value,
    /// `block` | `default_after`.
    pub policy_kind: String,
    /// Timeout of `default_after`, in milliseconds.
    pub timeout_ms: Option<i64>,
    /// `open` | `answered` | `expired`.
    pub status: String,
    /// The `Answer` as JSON.
    pub answer: Option<Value>,
    /// Who answered.
    pub answered_by: Option<String>,
    /// Whether the answer came from the default on expiry.
    pub applied_default: bool,
    /// When it was asked.
    pub asked_at: DateTime<Utc>,
    /// When it was answered.
    pub answered_at: Option<DateTime<Utc>>,
    /// Last applied event time.
    pub updated_at: DateTime<Utc>,
    /// Global position of the last applied event.
    pub last_position: i64,
}

impl QuestionInboxRow {
    /// Cursor pointing at this row.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.asked_at, self.question_id)
    }
}

/// A row of `orch.cost_ledger` (one task attempt).
#[derive(Debug, Clone, FromRow)]
pub struct CostLedgerRow {
    /// Attempt id.
    pub attempt_id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Owning task.
    pub task_id: Uuid,
    /// 1-based attempt number.
    pub attempt_no: i32,
    /// Task aggregate version this row reflects.
    pub version: i64,
    /// Task kind.
    pub task_kind: String,
    /// Worker that ran the attempt.
    pub worker: String,
    /// Model alias.
    pub model_alias: String,
    /// Effort, when set.
    pub effort: Option<String>,
    /// `running` | `succeeded` | `failed`.
    pub status: String,
    /// Failure class when it failed.
    pub failure_class: Option<String>,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Cache reads.
    pub cache_read_tokens: i64,
    /// Cache writes.
    pub cache_write_tokens: i64,
    /// Spend when known.
    pub cost_usd: Usd,
    /// Wall-clock milliseconds.
    pub wall_ms: i64,
    /// Attempt start.
    pub started_at: DateTime<Utc>,
    /// Attempt end.
    pub ended_at: Option<DateTime<Utc>>,
    /// Last applied event time.
    pub updated_at: DateTime<Utc>,
    /// Global position of the last applied event.
    pub last_position: i64,
}

impl CostLedgerRow {
    /// Cursor pointing at this row.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        Cursor::new(self.started_at, self.attempt_id)
    }
}

/// A row of `orch.task_log` (`TaskLogLineDto`).
#[derive(Debug, Clone, FromRow)]
pub struct TaskLogRow {
    /// Task the line belongs to.
    pub task_id: Uuid,
    /// Attempt number (`0` = task-level).
    pub attempt: i32,
    /// Strictly increasing within `(task_id, attempt)`.
    pub seq: i64,
    /// When the line was produced.
    pub at: DateTime<Utc>,
    /// `assistant` | `tool_call` | `tool_result` | `usage` | `system` | …
    pub kind: String,
    /// The line.
    pub payload: Value,
    /// Owning run, when known.
    pub run_id: Option<Uuid>,
    /// Attempt id, when known.
    pub attempt_id: Option<Uuid>,
    /// Domain event that produced the line (projection lines only).
    pub source_event_id: Option<Uuid>,
}

/// A row of `orch.artifacts` (`ArtifactDto`).
#[derive(Debug, Clone, FromRow)]
pub struct ArtifactRow {
    /// Artifact id.
    pub artifact_id: Uuid,
    /// Owning run.
    pub run_id: Uuid,
    /// Producing task, when any.
    pub task_id: Option<Uuid>,
    /// Producing attempt, when any.
    pub attempt_id: Option<Uuid>,
    /// `diff` | `file` | `pr_url` | `report` | `json` | `transcript`.
    pub kind: String,
    /// Where the bytes are.
    pub uri: String,
    /// Hex SHA-256 when known.
    pub sha256: Option<String>,
    /// Size in bytes when known.
    pub bytes: Option<i64>,
    /// `task` | `run`.
    pub produced_by: String,
    /// When it was produced.
    pub created_at: DateTime<Utc>,
    /// Last applied event time.
    pub updated_at: DateTime<Utc>,
    /// Global position of the last applied event.
    pub last_position: i64,
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// `GET /api/v1/runs`.
#[derive(Debug, Clone, Default)]
pub struct RunQuery {
    /// Keep only this status.
    pub status: Option<String>,
    /// Page cursor from a previous [`Page::next_cursor`].
    pub cursor: Option<String>,
    /// Page size (default [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`]).
    pub limit: Option<usize>,
}

/// `GET /api/v1/runs/{id}/tasks` and the TUI board.
#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    /// Keep only tasks of this run.
    pub run_id: Option<Uuid>,
    /// Keep only this status.
    pub status: Option<String>,
    /// Page cursor.
    pub cursor: Option<String>,
    /// Page size.
    pub limit: Option<usize>,
}

/// `GET /api/v1/questions`.
#[derive(Debug, Clone, Default)]
pub struct QuestionQuery {
    /// Keep only questions of this run.
    pub run_id: Option<Uuid>,
    /// Keep only this status (`open` for the inbox).
    pub status: Option<String>,
    /// Page cursor.
    pub cursor: Option<String>,
    /// Page size.
    pub limit: Option<usize>,
}

/// `GET /api/v1/tasks/{id}/log`.
#[derive(Debug, Clone)]
pub struct TaskLogQuery {
    /// The task.
    pub task_id: Uuid,
    /// Keep only this attempt.
    pub attempt: Option<i32>,
    /// Return lines with `seq > after_seq` (the SSE `Last-Event-ID`).
    pub after_seq: Option<u64>,
    /// Page size.
    pub limit: Option<usize>,
}

impl TaskLogQuery {
    /// Every line of `task_id`.
    #[must_use]
    pub const fn new(task_id: Uuid) -> Self {
        Self {
            task_id,
            attempt: None,
            after_seq: None,
            limit: None,
        }
    }
}

/// How `kevin cost` groups the ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CostGroupBy {
    /// One row per run.
    #[default]
    Run,
    /// One row per model alias.
    Model,
    /// One row per task kind.
    Kind,
}

impl CostGroupBy {
    /// The `snake_case` name used by `--group-by`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            CostGroupBy::Run => "run",
            CostGroupBy::Model => "model",
            CostGroupBy::Kind => "kind",
        }
    }

    const fn column(self) -> &'static str {
        match self {
            CostGroupBy::Run => "run_id::text",
            CostGroupBy::Model => "model_alias",
            CostGroupBy::Kind => "task_kind",
        }
    }
}

/// `GET /api/v1/cost`.
#[derive(Debug, Clone, Default)]
pub struct CostQuery {
    /// Keep only attempts started at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// Keep only this run.
    pub run_id: Option<Uuid>,
    /// Grouping.
    pub group_by: CostGroupBy,
}

/// One grouped row of [`CostReport`] (`CostRowDto`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostRow {
    /// Run id, model alias or task kind, depending on the grouping.
    pub key: String,
    /// Spend when known.
    pub usd: Option<Decimal>,
    /// Prompt tokens.
    pub input_tokens: i64,
    /// Completion tokens.
    pub output_tokens: i64,
    /// Attempts counted.
    pub attempts: i64,
}

/// `CostReportDto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostReport {
    /// Total spend when known.
    pub total_usd: Option<Decimal>,
    /// Total tokens (input + output).
    pub total_tokens: i64,
    /// The grouped rows, biggest spender first.
    pub rows: Vec<CostRow>,
}

// ---------------------------------------------------------------------------
// ReadModels
// ---------------------------------------------------------------------------

const RUN_COLUMNS: &str = "run_id, version, status, goal_text, goal_excerpt, cwd, repo_kind, mode, \
     mode_detail, requested_by, auto_approve_plans, budget, usage, cost_usd::text AS cost_usd, \
     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, wall_ms, planner_route, \
     understanding, plan, plan_revision, open_question_ids, task_ids, tasks_total, tasks_succeeded, \
     tasks_failed, tasks_cancelled, tasks_skipped, evaluation_id, evaluation_overall, \
     evaluation_verdict, budget_exhausted, failure_reason, failure_class, failure_message, \
     cancelled_by, cancel_reason, summary, artifacts, created_at, updated_at, last_position";

const TASK_COLUMNS: &str = "task_id, run_id, version, seq, kind, title, instructions, status, spec, \
     acceptance_criteria, depends_on, budget, route, route_worker, route_model, route_effort, \
     selection, attempts, attempt_count, usage, cost_usd::text AS cost_usd, input_tokens, \
     output_tokens, cache_read_tokens, cache_write_tokens, wall_ms, artifacts, summary, \
     failure_class, failure_message, awaiting_question_id, started_at, ended_at, created_at, \
     updated_at, last_position";

const QUESTION_COLUMNS: &str = "question_id, run_id, task_id, version, text, options, multi_select, \
     default_answer, policy, policy_kind, timeout_ms, status, answer, answered_by, applied_default, \
     asked_at, answered_at, updated_at, last_position";

const COST_COLUMNS: &str = "attempt_id, run_id, task_id, attempt_no, version, task_kind, worker, \
     model_alias, effort, status, failure_class, input_tokens, output_tokens, cache_read_tokens, \
     cache_write_tokens, cost_usd::text AS cost_usd, wall_ms, started_at, ended_at, updated_at, \
     last_position";

const ARTIFACT_COLUMNS: &str = "artifact_id, run_id, task_id, attempt_id, kind, uri, sha256, bytes, \
     produced_by, created_at, updated_at, last_position";

const LOG_COLUMNS: &str =
    "task_id, attempt, seq, at, kind, payload, run_id, attempt_id, source_event_id";

/// Typed queries over the `orch` read models.
#[derive(Debug, Clone)]
pub struct ReadModels {
    pool: PgPool,
}

impl ReadModels {
    /// Wraps a pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool these queries run on.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -- runs ---------------------------------------------------------------

    /// One run (`GET /api/v1/runs/{run_id}`).
    pub async fn run(&self, run_id: Uuid) -> Result<Option<RunOverviewRow>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM orch.run_overview WHERE run_id = $1");
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Runs, newest activity first (`GET /api/v1/runs`).
    pub async fn runs(&self, query: &RunQuery) -> Result<Page<RunOverviewRow>> {
        let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
        let limit = clamp_limit(query.limit);
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM orch.run_overview \
             WHERE ($1::text IS NULL OR status = $1) \
               AND ($2::timestamptz IS NULL OR (updated_at, run_id) < ($2, $3)) \
             ORDER BY updated_at DESC, run_id DESC LIMIT $4"
        );
        let items: Vec<RunOverviewRow> = sqlx::query_as(AssertSqlSafe(sql))
            .bind(query.status.as_deref())
            .bind(cursor.map(|c| c.at))
            .bind(cursor.map(|c| c.id))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(page(items, limit, RunOverviewRow::cursor))
    }

    // -- tasks --------------------------------------------------------------

    /// One task (`GET /api/v1/tasks/{task_id}`).
    pub async fn task(&self, task_id: Uuid) -> Result<Option<TaskBoardRow>> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM orch.task_board WHERE task_id = $1");
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Every task of a run in plan order (`GET /api/v1/runs/{id}/tasks`).
    pub async fn tasks_of_run(&self, run_id: Uuid) -> Result<Vec<TaskBoardRow>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM orch.task_board WHERE run_id = $1 \
             ORDER BY seq, created_at, task_id"
        );
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Tasks, newest activity first (TUI board, filtered lists).
    pub async fn tasks(&self, query: &TaskQuery) -> Result<Page<TaskBoardRow>> {
        let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
        let limit = clamp_limit(query.limit);
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM orch.task_board \
             WHERE ($1::uuid IS NULL OR run_id = $1) \
               AND ($2::text IS NULL OR status = $2) \
               AND ($3::timestamptz IS NULL OR (updated_at, task_id) < ($3, $4)) \
             ORDER BY updated_at DESC, task_id DESC LIMIT $5"
        );
        let items: Vec<TaskBoardRow> = sqlx::query_as(AssertSqlSafe(sql))
            .bind(query.run_id)
            .bind(query.status.as_deref())
            .bind(cursor.map(|c| c.at))
            .bind(cursor.map(|c| c.id))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(page(items, limit, TaskBoardRow::cursor))
    }

    // -- questions ----------------------------------------------------------

    /// One question (`GET /api/v1/questions/{question_id}`).
    pub async fn question(&self, question_id: Uuid) -> Result<Option<QuestionInboxRow>> {
        let sql =
            format!("SELECT {QUESTION_COLUMNS} FROM orch.question_inbox WHERE question_id = $1");
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(question_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The question inbox (`GET /api/v1/questions?status=open`).
    pub async fn questions(&self, query: &QuestionQuery) -> Result<Page<QuestionInboxRow>> {
        let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
        let limit = clamp_limit(query.limit);
        let sql = format!(
            "SELECT {QUESTION_COLUMNS} FROM orch.question_inbox \
             WHERE ($1::uuid IS NULL OR run_id = $1) \
               AND ($2::text IS NULL OR status = $2) \
               AND ($3::timestamptz IS NULL OR (asked_at, question_id) < ($3, $4)) \
             ORDER BY asked_at DESC, question_id DESC LIMIT $5"
        );
        let items: Vec<QuestionInboxRow> = sqlx::query_as(AssertSqlSafe(sql))
            .bind(query.run_id)
            .bind(query.status.as_deref())
            .bind(cursor.map(|c| c.at))
            .bind(cursor.map(|c| c.id))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(page(items, limit, QuestionInboxRow::cursor))
    }

    // -- task log -----------------------------------------------------------

    /// Transcript lines in `(attempt, seq)` order
    /// (`GET /api/v1/tasks/{task_id}/log`). The next cursor is the last `seq`.
    pub async fn task_log(&self, query: &TaskLogQuery) -> Result<Page<TaskLogRow>> {
        let limit = clamp_limit(query.limit);
        let after = i64::try_from(query.after_seq.unwrap_or(0)).unwrap_or(0);
        let sql = format!(
            "SELECT {LOG_COLUMNS} FROM orch.task_log \
             WHERE task_id = $1 AND ($2::int IS NULL OR attempt = $2) AND seq > $3 \
             ORDER BY attempt, seq LIMIT $4"
        );
        let items: Vec<TaskLogRow> = sqlx::query_as(AssertSqlSafe(sql))
            .bind(query.task_id)
            .bind(query.attempt)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        let next = (i64::try_from(items.len()).unwrap_or(0) == limit)
            .then(|| items.last().map(|row| row.seq.to_string()))
            .flatten();
        Ok(Page {
            items,
            next_cursor: next,
        })
    }

    /// Highest `seq` of `(task_id, attempt)` (`0` when the attempt has no lines).
    pub async fn task_log_head(&self, task_id: Uuid, attempt: i32) -> Result<u64> {
        let seq: Option<i64> = sqlx::query_scalar(
            "SELECT max(seq) FROM orch.task_log WHERE task_id = $1 AND attempt = $2",
        )
        .bind(task_id)
        .bind(attempt)
        .fetch_one(&self.pool)
        .await?;
        Ok(seq.unwrap_or(0).try_into().unwrap_or(0))
    }

    // -- artifacts ----------------------------------------------------------

    /// One artifact.
    pub async fn artifact(&self, artifact_id: Uuid) -> Result<Option<ArtifactRow>> {
        let sql = format!("SELECT {ARTIFACT_COLUMNS} FROM orch.artifacts WHERE artifact_id = $1");
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(artifact_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Artifacts of a task (`GET /api/v1/tasks/{task_id}/artifacts`).
    pub async fn artifacts_of_task(&self, task_id: Uuid) -> Result<Vec<ArtifactRow>> {
        let sql = format!(
            "SELECT {ARTIFACT_COLUMNS} FROM orch.artifacts WHERE task_id = $1 \
             ORDER BY created_at, artifact_id"
        );
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Artifacts of a run, task ones included.
    pub async fn artifacts_of_run(&self, run_id: Uuid) -> Result<Vec<ArtifactRow>> {
        let sql = format!(
            "SELECT {ARTIFACT_COLUMNS} FROM orch.artifacts WHERE run_id = $1 \
             ORDER BY created_at, artifact_id"
        );
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(run_id)
            .fetch_all(&self.pool)
            .await?)
    }

    // -- cost ---------------------------------------------------------------

    /// Raw ledger entries, newest first.
    pub async fn cost_entries(&self, query: &CostQuery) -> Result<Vec<CostLedgerRow>> {
        let sql = format!(
            "SELECT {COST_COLUMNS} FROM orch.cost_ledger \
             WHERE ($1::timestamptz IS NULL OR started_at >= $1) \
               AND ($2::uuid IS NULL OR run_id = $2) \
             ORDER BY started_at DESC, attempt_id DESC"
        );
        Ok(sqlx::query_as(AssertSqlSafe(sql))
            .bind(query.since)
            .bind(query.run_id)
            .fetch_all(&self.pool)
            .await?)
    }

    /// The grouped cost report (`GET /api/v1/cost`, `kevin cost`).
    pub async fn cost(&self, query: &CostQuery) -> Result<CostReport> {
        let group = query.group_by.column();
        let sql = format!(
            "SELECT {group} AS key, sum(cost_usd)::text AS usd, \
                    coalesce(sum(input_tokens), 0)::bigint AS input_tokens, \
                    coalesce(sum(output_tokens), 0)::bigint AS output_tokens, \
                    count(*) AS attempts \
             FROM orch.cost_ledger \
             WHERE ($1::timestamptz IS NULL OR started_at >= $1) \
               AND ($2::uuid IS NULL OR run_id = $2) \
             GROUP BY 1 ORDER BY sum(cost_usd) DESC NULLS LAST, 1"
        );
        let rows = sqlx::query(AssertSqlSafe(sql))
            .bind(query.since)
            .bind(query.run_id)
            .fetch_all(&self.pool)
            .await?;
        let mut report = CostReport {
            total_usd: None,
            total_tokens: 0,
            rows: Vec::with_capacity(rows.len()),
        };
        for row in rows {
            let usd: Usd = row.try_get("usd")?;
            let input_tokens: i64 = row.try_get("input_tokens")?;
            let output_tokens: i64 = row.try_get("output_tokens")?;
            if let Some(amount) = usd.0 {
                report.total_usd = Some(report.total_usd.unwrap_or_default() + amount);
            }
            report.total_tokens += input_tokens + output_tokens;
            report.rows.push(CostRow {
                key: row.try_get("key")?,
                usd: usd.0,
                input_tokens,
                output_tokens,
                attempts: row.try_get("attempts")?,
            });
        }
        Ok(report)
    }
}

/// Wraps `items` in a page, adding a cursor when the page is full.
fn page<T>(items: Vec<T>, limit: i64, cursor: impl Fn(&T) -> Cursor) -> Page<T> {
    let next_cursor = (i64::try_from(items.len()).unwrap_or(0) == limit)
        .then(|| items.last().map(|item| cursor(item).encode()))
        .flatten();
    Page { items, next_cursor }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trips() {
        let cursor = Cursor::new(
            Utc.timestamp_micros(1_767_225_600_123_456).unwrap(),
            Uuid::nil(),
        );
        assert_eq!(Cursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    #[test]
    fn invalid_cursors_are_rejected() {
        assert!(matches!(
            Cursor::decode("nonsense"),
            Err(ProjectionError::InvalidCursor { .. })
        ));
        assert!(matches!(
            Cursor::decode("12.not-a-uuid"),
            Err(ProjectionError::InvalidCursor { .. })
        ));
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT_I64);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(10_000)), i64::try_from(MAX_LIMIT).unwrap());
    }

    #[test]
    fn group_by_names_match_the_cli_flag() {
        assert_eq!(CostGroupBy::Run.as_str(), "run");
        assert_eq!(CostGroupBy::Model.as_str(), "model");
        assert_eq!(CostGroupBy::Kind.as_str(), "kind");
    }
}
