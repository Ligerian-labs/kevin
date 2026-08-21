//! `orch.run_overview` — the run list and run detail read model
//! (`RunDto`/`RunSummaryDto`, `plan/07-api-and-tui.md`).
//!
//! Source: the `run.*` stream only, so the row's `version` is the run
//! aggregate version and every statement is guarded by `version < $new` —
//! replaying an event is a no-op, and an out-of-order apply can never move the
//! row backwards.

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::ids::{QuestionId, TaskId};
use kevin_domain::run::{RunEvent, RunStatus, TaskOutcome};
use kevin_domain::values::{RepoKind, RunMode, Usage};
use sqlx::{PgConnection, PgPool};

use super::helpers::{UsageCols, at, excerpt, payload, position, version};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "run_overview";

/// Characters of `goal.text` kept in `goal_excerpt`.
const EXCERPT_CHARS: usize = 160;

/// Builds `orch.run_overview` from `run.*`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunOverview;

impl RunOverview {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Projection for RunOverview {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        event_type.starts_with("run.")
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        sqlx::query("TRUNCATE TABLE orch.run_overview")
            .execute(pool)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let run = event.envelope.aggregate_id;
        let ver = version(event);
        let pos = position(event);
        let ts = at(event);

        match payload::<RunEvent>(event)? {
            RunEvent::Started {
                goal,
                mode,
                budget,
                requested_by,
                auto_approve_plans,
                ..
            } => {
                let (mode_name, mode_detail) = mode_columns(&mode);
                sqlx::query(
                    "INSERT INTO orch.run_overview (
                         run_id, version, status, goal_text, goal_excerpt, cwd, repo_kind,
                         mode, mode_detail, requested_by, auto_approve_plans, budget,
                         usage, created_at, updated_at, last_position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                             '{}'::jsonb, $13, $13, $14)
                     ON CONFLICT (run_id) DO UPDATE SET
                         version = EXCLUDED.version,
                         status = EXCLUDED.status,
                         goal_text = EXCLUDED.goal_text,
                         goal_excerpt = EXCLUDED.goal_excerpt,
                         cwd = EXCLUDED.cwd,
                         repo_kind = EXCLUDED.repo_kind,
                         mode = EXCLUDED.mode,
                         mode_detail = EXCLUDED.mode_detail,
                         requested_by = EXCLUDED.requested_by,
                         auto_approve_plans = EXCLUDED.auto_approve_plans,
                         budget = EXCLUDED.budget,
                         created_at = EXCLUDED.created_at,
                         updated_at = EXCLUDED.updated_at,
                         last_position = EXCLUDED.last_position
                     WHERE orch.run_overview.version < EXCLUDED.version",
                )
                .bind(run)
                .bind(ver)
                .bind(RunStatus::Received.as_str())
                .bind(&goal.text)
                .bind(excerpt(&goal.text, EXCERPT_CHARS))
                .bind(goal.cwd.to_string_lossy().into_owned())
                .bind(repo_kind_str(goal.repo_kind))
                .bind(mode_name)
                .bind(mode_detail)
                .bind(&requested_by)
                .bind(auto_approve_plans)
                .bind(serde_json::to_value(&budget)?)
                .bind(ts)
                .bind(pos)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::UnderstandingStarted { planner_route } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = $5, planner_route = $6
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(RunStatus::Understanding.as_str())
                .bind(serde_json::to_value(&planner_route)?)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::UnderstandingCompleted {
                understanding,
                usage,
                question_ids,
            } => {
                let status = if question_ids.is_empty() {
                    RunStatus::Planning
                } else {
                    RunStatus::AwaitingAnswers
                };
                let open: Vec<uuid::Uuid> = question_ids.iter().map(QuestionId::as_uuid).collect();
                let u = UsageCols::new(&usage);
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = $5, understanding = $6, open_question_ids = $7,
                         input_tokens = input_tokens + $8, output_tokens = output_tokens + $9,
                         cache_read_tokens = cache_read_tokens + $10,
                         cache_write_tokens = cache_write_tokens + $11,
                         wall_ms = wall_ms + $12,
                         cost_usd = CASE WHEN cost_usd IS NULL AND $13::numeric IS NULL
                             THEN NULL ELSE coalesce(cost_usd, 0) + coalesce($13::numeric, 0) END
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(status.as_str())
                .bind(serde_json::to_value(&understanding)?)
                .bind(open)
                .bind(u.input)
                .bind(u.output)
                .bind(u.cache_read)
                .bind(u.cache_write)
                .bind(u.wall_ms)
                .bind(u.cost)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::QuestionAnswered { question_id, .. } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         open_question_ids = array_remove(open_question_ids, $5),
                         status = CASE
                             WHEN status = 'awaiting_answers'
                              AND cardinality(array_remove(open_question_ids, $5)) = 0
                             THEN 'planning' ELSE status END
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(question_id.as_uuid())
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::PlanProposed {
                plan,
                usage,
                revision,
            } => {
                let u = UsageCols::new(&usage);
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = $5, plan = $6, plan_revision = $7,
                         input_tokens = input_tokens + $8, output_tokens = output_tokens + $9,
                         cache_read_tokens = cache_read_tokens + $10,
                         cache_write_tokens = cache_write_tokens + $11,
                         wall_ms = wall_ms + $12,
                         cost_usd = CASE WHEN cost_usd IS NULL AND $13::numeric IS NULL
                             THEN NULL ELSE coalesce(cost_usd, 0) + coalesce($13::numeric, 0) END
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(RunStatus::AwaitingPlanApproval.as_str())
                .bind(serde_json::to_value(&plan)?)
                .bind(i32::from(revision))
                .bind(u.input)
                .bind(u.output)
                .bind(u.cache_read)
                .bind(u.cache_write)
                .bind(u.wall_ms)
                .bind(u.cost)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::PlanApproved { .. } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4, status = $5
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(RunStatus::Executing.as_str())
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::PlanRejected { .. } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'planning', plan_revision = plan_revision + 1
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::ExecutionStarted { task_ids } => {
                let ids: Vec<uuid::Uuid> = task_ids.iter().map(TaskId::as_uuid).collect();
                let total = i32::try_from(ids.len()).unwrap_or(i32::MAX);
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         task_ids = $5, tasks_total = $6
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(ids.clone())
                .bind(total)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::UsageRecorded { run_usage, .. } => {
                set_usage(conn, run, ver, pos, ts, &run_usage, None).await?;
            }
            RunEvent::TaskTerminalNoted {
                outcome,
                run_usage,
                all_terminal,
                ..
            } => {
                set_usage(
                    conn,
                    run,
                    ver,
                    pos,
                    ts,
                    &run_usage,
                    Some((outcome, all_terminal)),
                )
                .await?;
            }
            RunEvent::BudgetExhausted { dimension, .. } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4, budget_exhausted = $5
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(dimension.as_str())
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::Integrated { artifacts, summary } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'evaluating', artifacts = $5, summary = $6
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(serde_json::to_value(&artifacts)?)
                .bind(&summary)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::Evaluated {
                evaluation_id,
                overall,
                verdict,
            } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         evaluation_id = $5, evaluation_overall = $6, evaluation_verdict = $7
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(evaluation_id.as_uuid())
                .bind(overall)
                .bind(verdict.as_str())
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::Completed { summary, usage, .. } => {
                let u = UsageCols::new(&usage);
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'completed', summary = coalesce($5, summary),
                         usage = $6, input_tokens = $7, output_tokens = $8,
                         cache_read_tokens = $9, cache_write_tokens = $10, wall_ms = $11,
                         cost_usd = coalesce($12::numeric, cost_usd)
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(if summary.is_empty() {
                    None
                } else {
                    Some(summary)
                })
                .bind(serde_json::to_value(usage)?)
                .bind(u.input)
                .bind(u.output)
                .bind(u.cache_read)
                .bind(u.cache_write)
                .bind(u.wall_ms)
                .bind(u.cost)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::Failed {
                reason,
                class,
                usage,
                message,
            } => {
                let u = UsageCols::new(&usage);
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'failed', failure_reason = $5, failure_class = $6,
                         failure_message = $7, open_question_ids = '{}',
                         usage = $8, input_tokens = $9, output_tokens = $10,
                         cache_read_tokens = $11, cache_write_tokens = $12, wall_ms = $13,
                         cost_usd = coalesce($14::numeric, cost_usd)
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(reason.as_str())
                .bind(class.as_str())
                .bind(&message)
                .bind(serde_json::to_value(usage)?)
                .bind(u.input)
                .bind(u.output)
                .bind(u.cache_read)
                .bind(u.cache_write)
                .bind(u.wall_ms)
                .bind(u.cost)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::Cancelled { by, reason } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'cancelled', cancelled_by = $5, cancel_reason = $6,
                         open_question_ids = '{}'
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(&by)
                .bind(&reason)
                .execute(&mut *conn)
                .await?;
            }
            RunEvent::EvaluationRequested { .. } => {
                sqlx::query(
                    "UPDATE orch.run_overview SET
                         version = $2, last_position = $3, updated_at = $4
                     WHERE run_id = $1 AND version < $2",
                )
                .bind(run)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}

/// Applies the run's rolled-up usage (and, for `task_terminal_noted`, the task
/// counters and the `executing → integrating` transition).
async fn set_usage(
    conn: &mut PgConnection,
    run: uuid::Uuid,
    ver: i64,
    pos: i64,
    ts: chrono::DateTime<chrono::Utc>,
    usage: &Usage,
    terminal: Option<(TaskOutcome, bool)>,
) -> Result<()> {
    let u = UsageCols::new(usage);
    let (succeeded, failed, cancelled, skipped) = match terminal.map(|(o, _)| o) {
        Some(TaskOutcome::Succeeded) => (1, 0, 0, 0),
        Some(TaskOutcome::Failed) => (0, 1, 0, 0),
        Some(TaskOutcome::Cancelled) => (0, 0, 1, 0),
        Some(TaskOutcome::Skipped) => (0, 0, 0, 1),
        None => (0, 0, 0, 0),
    };
    let all_terminal = terminal.is_some_and(|(_, all)| all);
    sqlx::query(
        "UPDATE orch.run_overview SET
             version = $2, last_position = $3, updated_at = $4,
             usage = $5, input_tokens = $6, output_tokens = $7,
             cache_read_tokens = $8, cache_write_tokens = $9, wall_ms = $10,
             cost_usd = coalesce($11::numeric, cost_usd),
             tasks_succeeded = tasks_succeeded + $12,
             tasks_failed = tasks_failed + $13,
             tasks_cancelled = tasks_cancelled + $14,
             tasks_skipped = tasks_skipped + $15,
             status = CASE WHEN $16 AND status = 'executing' THEN 'integrating' ELSE status END
         WHERE run_id = $1 AND version < $2",
    )
    .bind(run)
    .bind(ver)
    .bind(pos)
    .bind(ts)
    .bind(serde_json::to_value(usage)?)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cache_read)
    .bind(u.cache_write)
    .bind(u.wall_ms)
    .bind(u.cost)
    .bind(succeeded)
    .bind(failed)
    .bind(cancelled)
    .bind(skipped)
    .bind(all_terminal)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `repo_kind` column, matching the serde form.
const fn repo_kind_str(kind: RepoKind) -> &'static str {
    match kind {
        RepoKind::Git => "git",
        RepoKind::Jj => "jj",
        RepoKind::None => "none",
    }
}

/// `mode` column + the Kohral ids (`mode_detail`).
fn mode_columns(mode: &RunMode) -> (&'static str, Option<serde_json::Value>) {
    match mode {
        RunMode::Interactive => ("interactive", None),
        RunMode::Headless => ("headless", None),
        RunMode::Kohral {
            turn_id,
            session_key,
            session_id,
        } => (
            "kohral",
            Some(serde_json::json!({
                "turn_id": turn_id,
                "session_key": session_key,
                "session_id": session_id,
            })),
        ),
    }
}
