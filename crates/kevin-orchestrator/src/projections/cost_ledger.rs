//! `orch.cost_ledger` — one row per task attempt, the source of `kevin cost`
//! and of `CostReportDto` (`plan/07-api-and-tui.md`).
//!
//! The row is opened by `task.attempt_started` (route, worker, model) and
//! closed by `task.attempt_succeeded` / `task.attempt_failed` (usage, cost).
//! `task_kind` is read back from the attempt's `task.created` event, which is
//! always committed before it — so a rebuild produces exactly the same rows as
//! the incremental apply.

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::task::TaskEvent;
use sqlx::{PgConnection, PgPool};

use super::helpers::{UsageCols, at, payload, position, run_id, task_kind, version};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "cost_ledger";

/// Builds `orch.cost_ledger` from `task.attempt_*`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CostLedger;

impl CostLedger {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Projection for CostLedger {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        matches!(
            event_type,
            "task.attempt_started" | "task.attempt_succeeded" | "task.attempt_failed"
        )
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        sqlx::query("TRUNCATE TABLE orch.cost_ledger")
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let task = event.envelope.aggregate_id;
        let ver = version(event);
        let pos = position(event);
        let ts = at(event);

        match payload::<TaskEvent>(event)? {
            TaskEvent::AttemptStarted {
                attempt_id,
                attempt_no,
                route,
                ..
            } => {
                let kind = task_kind(conn, task)
                    .await?
                    .unwrap_or_else(|| "unknown".to_owned());
                sqlx::query(
                    "INSERT INTO orch.cost_ledger (
                         attempt_id, run_id, task_id, attempt_no, version, task_kind,
                         worker, model_alias, effort, status, started_at, updated_at, last_position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'running', $10, $10, $11)
                     ON CONFLICT (attempt_id) DO UPDATE SET
                         run_id = EXCLUDED.run_id,
                         task_id = EXCLUDED.task_id,
                         attempt_no = EXCLUDED.attempt_no,
                         version = EXCLUDED.version,
                         task_kind = EXCLUDED.task_kind,
                         worker = EXCLUDED.worker,
                         model_alias = EXCLUDED.model_alias,
                         effort = EXCLUDED.effort,
                         status = EXCLUDED.status,
                         started_at = EXCLUDED.started_at,
                         updated_at = EXCLUDED.updated_at,
                         last_position = EXCLUDED.last_position
                     WHERE orch.cost_ledger.version < EXCLUDED.version",
                )
                .bind(attempt_id.as_uuid())
                .bind(run_id(event))
                .bind(task)
                .bind(i32::from(attempt_no))
                .bind(ver)
                .bind(kind)
                .bind(route.worker.to_string())
                .bind(route.model.to_string())
                .bind(route.effort.map(|e| e.to_string()))
                .bind(ts)
                .bind(pos)
                .execute(&mut *conn)
                .await?;
            }
            TaskEvent::AttemptSucceeded {
                attempt_id, usage, ..
            } => {
                close(
                    conn,
                    attempt_id.as_uuid(),
                    ver,
                    pos,
                    ts,
                    &usage,
                    "succeeded",
                    None,
                )
                .await?;
            }
            TaskEvent::AttemptFailed {
                attempt_id,
                class,
                usage,
                ..
            } => {
                close(
                    conn,
                    attempt_id.as_uuid(),
                    ver,
                    pos,
                    ts,
                    &usage,
                    "failed",
                    Some(class.as_str()),
                )
                .await?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Writes the attempt's final usage.
#[allow(clippy::too_many_arguments)]
async fn close(
    conn: &mut PgConnection,
    attempt_id: uuid::Uuid,
    ver: i64,
    pos: i64,
    ts: chrono::DateTime<chrono::Utc>,
    usage: &kevin_domain::values::Usage,
    status: &str,
    failure_class: Option<&str>,
) -> Result<()> {
    let u = UsageCols::new(usage);
    sqlx::query(
        "UPDATE orch.cost_ledger SET
             version = $2, last_position = $3, updated_at = $4, ended_at = $4,
             status = $5, failure_class = $6,
             input_tokens = $7, output_tokens = $8, cache_read_tokens = $9,
             cache_write_tokens = $10, wall_ms = $11, cost_usd = $12::numeric
         WHERE attempt_id = $1 AND version < $2",
    )
    .bind(attempt_id)
    .bind(ver)
    .bind(pos)
    .bind(ts)
    .bind(status)
    .bind(failure_class)
    .bind(u.input)
    .bind(u.output)
    .bind(u.cache_read)
    .bind(u.cache_write)
    .bind(u.wall_ms)
    .bind(u.cost)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
