//! `orch.question_inbox` — open and answered clarification questions
//! (`QuestionDto`, `plan/07-api-and-tui.md`; also the source of Kohral's
//! `input_required`).

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::question::QuestionEvent;
use kevin_domain::values::QuestionPolicy;
use sqlx::{PgConnection, PgPool};

use super::helpers::{at, payload, position, version};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "question_inbox";

/// Builds `orch.question_inbox` from `question.*`.
#[derive(Debug, Default, Clone, Copy)]
pub struct QuestionInbox;

impl QuestionInbox {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Projection for QuestionInbox {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        event_type.starts_with("question.")
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        sqlx::query("TRUNCATE TABLE orch.question_inbox")
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let question = event.envelope.aggregate_id;
        let ver = version(event);
        let pos = position(event);
        let ts = at(event);

        match payload::<QuestionEvent>(event)? {
            QuestionEvent::Asked {
                run_id,
                task_id,
                text,
                options,
                multi_select,
                default,
                policy,
                ..
            } => {
                let (policy_kind, timeout_ms) = match policy {
                    QuestionPolicy::Block => ("block", None),
                    QuestionPolicy::DefaultAfter { timeout } => (
                        "default_after",
                        Some(i64::try_from(timeout.as_millis()).unwrap_or(i64::MAX)),
                    ),
                };
                sqlx::query(
                    "INSERT INTO orch.question_inbox (
                         question_id, run_id, task_id, version, text, options, multi_select,
                         default_answer, policy, policy_kind, timeout_ms, status,
                         asked_at, updated_at, last_position)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'open', $12, $12, $13)
                     ON CONFLICT (question_id) DO UPDATE SET
                         run_id = EXCLUDED.run_id,
                         task_id = EXCLUDED.task_id,
                         version = EXCLUDED.version,
                         text = EXCLUDED.text,
                         options = EXCLUDED.options,
                         multi_select = EXCLUDED.multi_select,
                         default_answer = EXCLUDED.default_answer,
                         policy = EXCLUDED.policy,
                         policy_kind = EXCLUDED.policy_kind,
                         timeout_ms = EXCLUDED.timeout_ms,
                         status = EXCLUDED.status,
                         asked_at = EXCLUDED.asked_at,
                         updated_at = EXCLUDED.updated_at,
                         last_position = EXCLUDED.last_position
                     WHERE orch.question_inbox.version < EXCLUDED.version",
                )
                .bind(question)
                .bind(run_id.as_uuid())
                .bind(task_id.map(|t| t.as_uuid()))
                .bind(ver)
                .bind(&text)
                .bind(serde_json::to_value(&options)?)
                .bind(multi_select)
                .bind(default.as_ref().map(serde_json::to_value).transpose()?)
                .bind(serde_json::to_value(policy)?)
                .bind(policy_kind)
                .bind(timeout_ms)
                .bind(ts)
                .bind(pos)
                .execute(&mut *conn)
                .await?;
            }
            QuestionEvent::Answered {
                answer,
                answered_by,
            } => {
                sqlx::query(
                    "UPDATE orch.question_inbox SET
                         version = $2, last_position = $3, updated_at = $4,
                         status = 'answered', answer = $5, answered_by = $6, answered_at = $4
                     WHERE question_id = $1 AND version < $2",
                )
                .bind(question)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(serde_json::to_value(&answer)?)
                .bind(&answered_by)
                .execute(&mut *conn)
                .await?;
            }
            QuestionEvent::Expired { applied_default } => {
                // `question.expired` is followed by `question.answered` when a
                // default exists; the status only becomes `expired` otherwise.
                sqlx::query(
                    "UPDATE orch.question_inbox SET
                         version = $2, last_position = $3, updated_at = $4,
                         applied_default = $5,
                         status = CASE WHEN $5 THEN status ELSE 'expired' END
                     WHERE question_id = $1 AND version < $2",
                )
                .bind(question)
                .bind(ver)
                .bind(pos)
                .bind(ts)
                .bind(applied_default)
                .execute(&mut *conn)
                .await?;
            }
        }
        Ok(())
    }
}
