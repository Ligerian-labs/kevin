//! `orch.artifacts` — every artifact a task attempt or the integration step
//! produced (`ArtifactDto`, `GET /api/v1/artifacts/{id}`).
//!
//! Rows are keyed by the artifact id, so re-applying an event is a plain
//! upsert of the same values.

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::run::RunEvent;
use kevin_domain::task::TaskEvent;
use kevin_domain::values::ArtifactRef;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::helpers::{at, payload, position, run_id};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "artifacts";

/// Builds `orch.artifacts` from `task.attempt_succeeded` and `run.integrated`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Artifacts;

impl Artifacts {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Projection for Artifacts {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        matches!(event_type, "task.attempt_succeeded" | "run.integrated")
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        sqlx::query("TRUNCATE TABLE orch.artifacts")
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let pos = position(event);
        let ts = at(event);
        let run = run_id(event);

        match event.envelope.event_type {
            "task.attempt_succeeded" => {
                if let TaskEvent::AttemptSucceeded {
                    attempt_id,
                    artifacts,
                    ..
                } = payload::<TaskEvent>(event)?
                {
                    for artifact in &artifacts {
                        insert(
                            conn,
                            artifact,
                            run,
                            Some(event.envelope.aggregate_id),
                            Some(attempt_id.as_uuid()),
                            "task",
                            ts,
                            pos,
                        )
                        .await?;
                    }
                }
            }
            "run.integrated" => {
                if let RunEvent::Integrated { artifacts, .. } = payload::<RunEvent>(event)? {
                    for artifact in &artifacts {
                        insert(conn, artifact, run, None, None, "run", ts, pos).await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert(
    conn: &mut PgConnection,
    artifact: &ArtifactRef,
    run: Uuid,
    task: Option<Uuid>,
    attempt: Option<Uuid>,
    produced_by: &str,
    ts: chrono::DateTime<chrono::Utc>,
    pos: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO orch.artifacts (
             artifact_id, run_id, task_id, attempt_id, kind, uri, sha256, bytes,
             produced_by, created_at, updated_at, last_position)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10, $11)
         ON CONFLICT (artifact_id) DO UPDATE SET
             run_id = EXCLUDED.run_id,
             task_id = EXCLUDED.task_id,
             attempt_id = EXCLUDED.attempt_id,
             kind = EXCLUDED.kind,
             uri = EXCLUDED.uri,
             sha256 = EXCLUDED.sha256,
             bytes = EXCLUDED.bytes,
             produced_by = EXCLUDED.produced_by,
             updated_at = EXCLUDED.updated_at,
             last_position = EXCLUDED.last_position
         WHERE orch.artifacts.last_position <= EXCLUDED.last_position",
    )
    .bind(artifact.id.as_uuid())
    .bind(run)
    .bind(task)
    .bind(attempt)
    .bind(
        serde_json::to_value(artifact.kind)?
            .as_str()
            .unwrap_or("file")
            .to_owned(),
    )
    .bind(&artifact.uri)
    .bind(artifact.sha256.as_deref())
    .bind(artifact.bytes.map(|b| i64::try_from(b).unwrap_or(i64::MAX)))
    .bind(produced_by)
    .bind(ts)
    .bind(pos)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
