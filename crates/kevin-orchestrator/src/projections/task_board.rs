//! `orch.task_board` — the task board / task detail read model (`TaskDto`,
//! `AttemptDto`, `plan/07-api-and-tui.md`).
//!
//! Source: the `task.*` stream. The row carries the task aggregate `version`
//! and every statement is guarded by `version < $new`, so replays are no-ops
//! and the attempt array can be appended to safely. `attempts` is a JSONB
//! array of `AttemptDto`-shaped objects; the task's usage columns are
//! recomputed from it after every change, which keeps them consistent whatever
//! order a rebuild takes.

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::ids::TaskId;
use kevin_domain::task::{TaskEvent, TaskStatus};
use kevin_domain::values::Usage;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::helpers::{at, payload, position, version};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "task_board";

/// Builds `orch.task_board` from `task.*`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskBoard;

impl TaskBoard {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Patches the attempt whose `id` is `$5` with the object `$6`, keeping order.
const PATCH_ATTEMPTS: &str = "attempts = coalesce((
             SELECT jsonb_agg(CASE WHEN e->>'id' = $5 THEN e || $6 ELSE e END ORDER BY ord)
             FROM jsonb_array_elements(attempts) WITH ORDINALITY AS patched(e, ord)), '[]'::jsonb)";

#[async_trait]
impl Projection for TaskBoard {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        event_type.starts_with("task.")
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        sqlx::query("TRUNCATE TABLE orch.task_board")
            .execute(pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let task = event.envelope.aggregate_id;
        let ver = version(event);
        let pos = position(event);
        let ts = at(event);

        match payload::<TaskEvent>(event)? {
            TaskEvent::Created {
                run_id: run,
                kind,
                spec,
                budget,
                ..
            } => {
                let depends_on: Vec<Uuid> = spec.depends_on.iter().map(TaskId::as_uuid).collect();
                sqlx::query(
                    "INSERT INTO orch.task_board (
                         task_id, run_id, version, seq, kind, title, instructions, status,
                         spec, acceptance_criteria, depends_on, budget,
                         created_at, updated_at, last_position)
                     VALUES ($1, $2, $3,
                             (SELECT count(*) FROM orch.task_board WHERE run_id = $2)::int,
                             $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $13)
                     ON CONFLICT (task_id) DO UPDATE SET
                         run_id = EXCLUDED.run_id,
                         version = EXCLUDED.version,
                         kind = EXCLUDED.kind,
                         title = EXCLUDED.title,
                         instructions = EXCLUDED.instructions,
                         status = EXCLUDED.status,
                         spec = EXCLUDED.spec,
                         acceptance_criteria = EXCLUDED.acceptance_criteria,
                         depends_on = EXCLUDED.depends_on,
                         budget = EXCLUDED.budget,
                         created_at = EXCLUDED.created_at,
                         updated_at = EXCLUDED.updated_at,
                         last_position = EXCLUDED.last_position
                     WHERE orch.task_board.version < EXCLUDED.version",
                )
                .bind(task)
                .bind(run.as_uuid())
                .bind(ver)
                .bind(kind.to_string())
                .bind(&spec.title)
                .bind(&spec.instructions)
                .bind(TaskStatus::Pending.as_str())
                .bind(serde_json::to_value(&spec)?)
                .bind(serde_json::to_value(&spec.acceptance_criteria)?)
                .bind(depends_on)
                .bind(serde_json::to_value(&budget)?)
                .bind(ts)
                .bind(pos)
                .execute(&mut *conn)
                .await?;
            }
            TaskEvent::Routed { route, selection } => {
                sqlx::query(
                    "UPDATE orch.task_board SET
                         version = $2, last_position = $3, updated_at = $4, status = $5,
                         route = $6, route_worker = $7, route_model = $8, route_effort = $9,
                         selection = $10
                     WHERE task_id = $1 AND version < $2",
                )
                .bind(task)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(TaskStatus::Routed.as_str())
                .bind(serde_json::to_value(&route)?)
                .bind(route.worker.to_string())
                .bind(route.model.to_string())
                .bind(route.effort.map(|e| e.to_string()))
                .bind(serde_json::to_value(&selection)?)
                .execute(&mut *conn)
                .await?;
            }
            TaskEvent::AttemptStarted {
                attempt_id,
                attempt_no,
                route,
                workspace,
                worker_session_id,
            } => {
                let attempt = json!({
                    "id": attempt_id.as_uuid().to_string(),
                    "no": attempt_no,
                    "route": serde_json::to_value(&route)?,
                    "status": "running",
                    "workspace": serde_json::to_value(&workspace)?,
                    "worker_session_id": worker_session_id,
                    "started_at": ts,
                    "ended_at": Value::Null,
                    "usage": serde_json::to_value(Usage::ZERO)?,
                    "summary": Value::Null,
                    "failure": Value::Null,
                    "last_log_seq": 0,
                });
                sqlx::query(
                    "UPDATE orch.task_board SET
                         version = $2, last_position = $3, updated_at = $4, status = $5,
                         attempts = attempts || $6, attempt_count = attempt_count + 1,
                         started_at = coalesce(started_at, $4), ended_at = NULL,
                         awaiting_question_id = NULL,
                         route = $7, route_worker = $8, route_model = $9, route_effort = $10
                     WHERE task_id = $1 AND version < $2",
                )
                .bind(task)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(TaskStatus::Running.as_str())
                .bind(json!([attempt]))
                .bind(serde_json::to_value(&route)?)
                .bind(route.worker.to_string())
                .bind(route.model.to_string())
                .bind(route.effort.map(|e| e.to_string()))
                .execute(&mut *conn)
                .await?;
                recompute_usage(conn, task).await?;
            }
            TaskEvent::Progressed {
                attempt_id,
                summary,
                usage_delta,
                log_seq,
            } => {
                let mut patch = json!({
                    "summary": summary,
                    "last_log_seq": log_seq,
                    "usage": Value::Null, // replaced right below by the accumulated value
                });
                accumulate_attempt_usage(
                    conn,
                    task,
                    attempt_id.as_uuid(),
                    &usage_delta,
                    &mut patch,
                )
                .await?;
                attempt_update(conn, task, ver, pos, ts, attempt_id.as_uuid(), patch)
                    .summary(summary.clone())
                    .exec()
                    .await?;
                recompute_usage(conn, task).await?;
            }
            TaskEvent::InputRequested {
                attempt_id,
                question_id,
            } => {
                attempt_update(
                    conn,
                    task,
                    ver,
                    pos,
                    ts,
                    attempt_id.as_uuid(),
                    json!({ "status": "awaiting_input", "pending_question": question_id.as_uuid().to_string() }),
                )
                .status(TaskStatus::AwaitingInput.as_str())
                .awaiting_question(Some(question_id.as_uuid()))
                .exec()
                .await?;
            }
            TaskEvent::InputProvided { attempt_id, .. } => {
                attempt_update(
                    conn,
                    task,
                    ver,
                    pos,
                    ts,
                    attempt_id.as_uuid(),
                    json!({ "status": "running", "pending_question": Value::Null }),
                )
                .status(TaskStatus::Running.as_str())
                .awaiting_question(None)
                .exec()
                .await?;
            }
            TaskEvent::AttemptSucceeded {
                attempt_id,
                artifacts,
                summary,
                usage,
            } => {
                let patch = json!({
                    "status": "succeeded",
                    "ended_at": ts,
                    "summary": summary.clone(),
                    "pending_question": Value::Null,
                    "usage": serde_json::to_value(usage)?,
                });
                attempt_update(conn, task, ver, pos, ts, attempt_id.as_uuid(), patch)
                    .status(TaskStatus::Succeeded.as_str())
                    .summary(summary)
                    .ended()
                    .artifacts(serde_json::to_value(&artifacts)?)
                    .awaiting_question(None)
                    .exec()
                    .await?;
                recompute_usage(conn, task).await?;
            }
            TaskEvent::AttemptFailed {
                attempt_id,
                class,
                message,
                usage,
                ..
            } => {
                let patch = json!({
                    "status": "failed",
                    "ended_at": ts,
                    "pending_question": Value::Null,
                    "usage": serde_json::to_value(usage)?,
                    "failure": { "class": class.as_str(), "message": message.clone() },
                });
                attempt_update(conn, task, ver, pos, ts, attempt_id.as_uuid(), patch)
                    .status(TaskStatus::Failed.as_str())
                    .failure(class.as_str(), message)
                    .ended()
                    .awaiting_question(None)
                    .exec()
                    .await?;
                recompute_usage(conn, task).await?;
            }
            TaskEvent::Retried { .. } => {
                sqlx::query(
                    "UPDATE orch.task_board SET
                         version = $2, last_position = $3, updated_at = $4, status = $5
                     WHERE task_id = $1 AND version < $2",
                )
                .bind(task)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(TaskStatus::Routed.as_str())
                .execute(&mut *conn)
                .await?;
            }
            TaskEvent::Cancelled { reason } => {
                // The domain marks a still-active attempt as failed/cancelled.
                let active = active_attempt(conn, task).await?;
                let patch = json!({
                    "status": "failed",
                    "ended_at": ts,
                    "pending_question": Value::Null,
                    "failure": { "class": "cancelled", "message": reason.clone() },
                });
                attempt_update(
                    conn,
                    task,
                    ver,
                    pos,
                    ts,
                    active.unwrap_or(Uuid::nil()),
                    patch,
                )
                .status(TaskStatus::Cancelled.as_str())
                .failure("cancelled", reason)
                .ended()
                .awaiting_question(None)
                .exec()
                .await?;
            }
            TaskEvent::Skipped { reason } => {
                sqlx::query(
                    "UPDATE orch.task_board SET
                         version = $2, last_position = $3, updated_at = $4, status = $5,
                         failure_message = $6, ended_at = $4
                     WHERE task_id = $1 AND version < $2",
                )
                .bind(task)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(TaskStatus::Skipped.as_str())
                .bind(&reason)
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Id of the attempt that is still `running`/`awaiting_input`, if any.
async fn active_attempt(conn: &mut PgConnection, task: Uuid) -> Result<Option<Uuid>> {
    let id: Option<String> = sqlx::query_scalar(
        "SELECT e->>'id' FROM orch.task_board, jsonb_array_elements(attempts) AS e
         WHERE task_id = $1 AND e->>'status' IN ('running', 'awaiting_input')
         ORDER BY (e->>'no')::int DESC LIMIT 1",
    )
    .bind(task)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(id.and_then(|s| Uuid::parse_str(&s).ok()))
}

/// Adds `delta` to the attempt's current usage and writes the result into `patch`.
async fn accumulate_attempt_usage(
    conn: &mut PgConnection,
    task: Uuid,
    attempt_id: Uuid,
    delta: &Usage,
    patch: &mut Value,
) -> Result<()> {
    let current: Option<Value> = sqlx::query_scalar(
        "SELECT e->'usage' FROM orch.task_board, jsonb_array_elements(attempts) AS e
         WHERE task_id = $1 AND e->>'id' = $2 LIMIT 1",
    )
    .bind(task)
    .bind(attempt_id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    let current = current
        .and_then(|v| serde_json::from_value::<Usage>(v).ok())
        .unwrap_or(Usage::ZERO);
    patch["usage"] = serde_json::to_value(current + *delta)?;
    Ok(())
}

/// Recomputes the task's usage columns from the attempts array (derived data,
/// so it needs no version guard).
async fn recompute_usage(conn: &mut PgConnection, task: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE orch.task_board t SET
             input_tokens = c.input_tokens,
             output_tokens = c.output_tokens,
             cache_read_tokens = c.cache_read_tokens,
             cache_write_tokens = c.cache_write_tokens,
             wall_ms = c.wall_ms,
             cost_usd = c.cost_usd,
             usage = jsonb_build_object(
                 'input_tokens', c.input_tokens,
                 'output_tokens', c.output_tokens,
                 'cache_read_tokens', c.cache_read_tokens,
                 'cache_write_tokens', c.cache_write_tokens,
                 'wall_ms', c.wall_ms,
                 'cost_usd', c.cost_usd::text)
         FROM (
             SELECT
                 coalesce(sum((e->'usage'->>'input_tokens')::bigint), 0) AS input_tokens,
                 coalesce(sum((e->'usage'->>'output_tokens')::bigint), 0) AS output_tokens,
                 coalesce(sum((e->'usage'->>'cache_read_tokens')::bigint), 0) AS cache_read_tokens,
                 coalesce(sum((e->'usage'->>'cache_write_tokens')::bigint), 0) AS cache_write_tokens,
                 coalesce(sum((e->'usage'->>'wall_ms')::bigint), 0) AS wall_ms,
                 CASE WHEN count(e->'usage'->>'cost_usd') = 0 THEN NULL
                      ELSE sum((e->'usage'->>'cost_usd')::numeric) END AS cost_usd
             FROM orch.task_board b, jsonb_array_elements(b.attempts) AS e
             WHERE b.task_id = $1
         ) c
         WHERE t.task_id = $1",
    )
    .bind(task)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// One fixed statement covering every "patch an attempt and the task row" case
// ---------------------------------------------------------------------------

struct AttemptUpdate<'c> {
    conn: &'c mut PgConnection,
    task: Uuid,
    ver: i64,
    pos: i64,
    ts: chrono::DateTime<chrono::Utc>,
    attempt_id: Uuid,
    patch: Value,
    status: Option<&'static str>,
    ended: bool,
    summary: Option<String>,
    failure: Option<(&'static str, String)>,
    artifacts: Option<Value>,
    set_awaiting: bool,
    awaiting: Option<Uuid>,
}

fn attempt_update(
    conn: &mut PgConnection,
    task: Uuid,
    ver: i64,
    pos: i64,
    ts: chrono::DateTime<chrono::Utc>,
    attempt_id: Uuid,
    patch: Value,
) -> AttemptUpdate<'_> {
    AttemptUpdate {
        conn,
        task,
        ver,
        pos,
        ts,
        attempt_id,
        patch,
        status: None,
        ended: false,
        summary: None,
        failure: None,
        artifacts: None,
        set_awaiting: false,
        awaiting: None,
    }
}

impl AttemptUpdate<'_> {
    fn status(mut self, status: &'static str) -> Self {
        self.status = Some(status);
        self
    }

    fn ended(mut self) -> Self {
        self.ended = true;
        self
    }

    fn summary(mut self, summary: String) -> Self {
        self.summary = Some(summary);
        self
    }

    fn failure(mut self, class: &'static str, message: String) -> Self {
        self.failure = Some((class, message));
        self
    }

    fn artifacts(mut self, artifacts: Value) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    fn awaiting_question(mut self, question: Option<Uuid>) -> Self {
        self.set_awaiting = true;
        self.awaiting = question;
        self
    }

    async fn exec(self) -> Result<()> {
        let sql = format!(
            "UPDATE orch.task_board SET
                 version = $2, last_position = $3, updated_at = $4,
                 {PATCH_ATTEMPTS},
                 status = coalesce($7, status),
                 ended_at = CASE WHEN $8 THEN $4 ELSE ended_at END,
                 summary = coalesce($9, summary),
                 failure_class = CASE WHEN $10::text IS NULL THEN failure_class ELSE $10 END,
                 failure_message = CASE WHEN $10::text IS NULL THEN failure_message ELSE $11 END,
                 artifacts = CASE WHEN $12::jsonb IS NULL THEN artifacts ELSE artifacts || $12 END,
                 awaiting_question_id = CASE WHEN $13 THEN $14 ELSE awaiting_question_id END
             WHERE task_id = $1 AND version < $2"
        );
        let (class, message) = match self.failure {
            Some((c, m)) => (Some(c), Some(m)),
            None => (None, None),
        };
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(self.task)
            .bind(self.ver)
            .bind(self.pos)
            .bind(self.ts)
            .bind(self.attempt_id.to_string())
            .bind(self.patch)
            .bind(self.status)
            .bind(self.ended)
            .bind(self.summary)
            .bind(class)
            .bind(message)
            .bind(self.artifacts)
            .bind(self.set_awaiting)
            .bind(self.awaiting)
            .execute(&mut *self.conn)
            .await?;
        Ok(())
    }
}
