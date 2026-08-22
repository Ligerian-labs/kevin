//! `KohralLedgerProjection` — the only writer of `kohral.runs_ledger` after
//! acceptance (`plan/08-kohral-runtime.md` §2).
//!
//! It folds the run's own events into the durable turn status. Two invariants
//! shape every statement here, because Kohral treats a violation of either as
//! a `runtime_protocol_error`:
//!
//! - `partial_output` only ever grows (`partial_output = partial_output || …`),
//! - `seq` only ever increases.
//!
//! Consequences:
//!
//! - every write is guarded by `last_position < $position`, so a projection
//!   rebuild cannot append the same narrative twice;
//! - every write is guarded by `status NOT IN (terminal)`, so a late event for
//!   a run the boot sweep already failed as `runtime_restarted` is ignored —
//!   the contract says a terminal turn stays terminal;
//! - the final answer is merged with Hermes'
//!   [`reconcile_completed_output`](crate::ledger::reconcile_completed_output),
//!   which appends only the unseen suffix.
//!
//! A Kohral worker polling while the projection lags therefore sees a stale
//! but *consistent* snapshot, never a regression.

use std::collections::BTreeMap;

use async_trait::async_trait;
use kevin_bus::BusEvent;
use kevin_domain::run::RunEvent;
use kevin_domain::task::TaskEvent;
use kevin_domain::{FailureClass, RunFailureReason, Usage};
use kevin_orchestrator::projections::{Projection, ProjectionError, Result as ProjectionResult};
use serde::de::DeserializeOwned;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::ledger::{RUNTIME_RESTARTED, TurnStatus, reconcile_completed_output};
use crate::render;

/// Checkpoint name in `core.projection_checkpoints`.
pub const NAME: &str = "kohral_runs_ledger";

/// Event types the ledger folds.
const HANDLED: &[&str] = &[
    "run.understanding_started",
    "run.understanding_completed",
    "run.plan_proposed",
    "run.execution_started",
    "run.usage_recorded",
    "run.task_terminal_noted",
    "run.budget_exhausted",
    "run.integrated",
    "run.completed",
    "run.failed",
    "run.cancelled",
    "task.progressed",
    "task.attempt_succeeded",
    "task.attempt_failed",
    "task.retried",
    "task.skipped",
    "question.answered",
];

/// How the ledger renders progress into `partial_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Narrative {
    /// Kevin's Markdown progress narrative *and* the final answer — what an
    /// operator watching a Kohral conversation wants to see (`plan/08` §1.3).
    #[default]
    Full,
    /// Only the final answer, so `partial_output` carries exactly what the
    /// agent replied and nothing else. This is Hermes' own semantics, and it
    /// is what the conformance profile uses: `contract.py` asserts the output
    /// of the `reply deterministically` turn is byte-for-byte `kohral-ok`.
    AnswerOnly,
}

impl Narrative {
    /// `Full` unless the conformance profile is active
    /// (`workers.fake.enabled`, `plan/08` §1.9).
    #[must_use]
    pub const fn for_config(config: &kevin_config::KevinConfig) -> Self {
        if config.workers.fake.enabled {
            Narrative::AnswerOnly
        } else {
            Narrative::Full
        }
    }

    const fn renders_progress(self) -> bool {
        matches!(self, Narrative::Full)
    }
}

/// The projection.
#[derive(Debug)]
pub struct KohralLedgerProjection {
    narrative: Narrative,
    /// Task titles seen in this process, so a progress line can name the task
    /// without a query. Cold entries fall back to `core.events`.
    titles: BTreeMap<Uuid, String>,
}

impl KohralLedgerProjection {
    /// A projection rendering `narrative`.
    #[must_use]
    pub const fn new(narrative: Narrative) -> Self {
        Self {
            narrative,
            titles: BTreeMap::new(),
        }
    }

    /// The effect one event has on the ledger row.
    async fn effect(
        &mut self,
        event: &BusEvent,
        conn: &mut PgConnection,
    ) -> ProjectionResult<Effect> {
        let event_type = event.envelope.event_type;
        let mut effect = Effect::new(event_type);
        match event_type {
            "question.answered" => {
                let QuestionAnswered {
                    answer,
                    answered_by,
                } = payload(event)?;
                if answered_by == "default" {
                    let question = question_text(conn, event.envelope.aggregate_id).await?;
                    effect.append = render::assumption(&question, &answer);
                }
                return Ok(effect);
            }
            "task.progressed"
            | "task.attempt_succeeded"
            | "task.attempt_failed"
            | "task.retried"
            | "task.skipped" => {
                let task_id = event.envelope.aggregate_id;
                let title = self.title(conn, task_id).await?;
                let task: TaskEvent = payload(event)?;
                effect.usage_delta = task_usage(&task);
                effect.append = render::task_line(&title, &task);
                effect.status = Some(TurnStatus::Running);
                return Ok(effect);
            }
            _ => {}
        }

        match payload::<RunEvent>(event)? {
            RunEvent::UnderstandingStarted { .. } => {
                effect.status = Some(TurnStatus::Running);
            }
            RunEvent::UnderstandingCompleted {
                understanding,
                usage,
                ..
            } => {
                effect.status = Some(TurnStatus::Running);
                effect.usage_delta = Some(usage);
                effect.append = render::understanding(&understanding);
            }
            RunEvent::PlanProposed { plan, usage, .. } => {
                effect.status = Some(TurnStatus::Running);
                effect.usage_delta = Some(usage);
                effect.append = render::plan(&plan);
            }
            RunEvent::ExecutionStarted { task_ids } => {
                effect.status = Some(TurnStatus::Running);
                effect.append = render::execution_started(task_ids.len());
            }
            RunEvent::UsageRecorded { run_usage, .. }
            | RunEvent::TaskTerminalNoted { run_usage, .. } => {
                effect.usage_total = Some(run_usage);
            }
            RunEvent::BudgetExhausted {
                dimension,
                limit,
                actual,
            } => {
                effect.append = render::budget_exhausted(dimension, limit, actual);
            }
            RunEvent::Integrated { summary, artifacts } => {
                effect.append = render::integration(&summary, &artifacts);
            }
            RunEvent::Completed { summary, usage, .. } => {
                effect.status = Some(TurnStatus::Completed);
                effect.usage_total = Some(usage);
                effect.answer = Some(summary);
            }
            RunEvent::Failed {
                reason,
                class,
                usage,
                message,
            } => {
                effect.status = Some(TurnStatus::Failed);
                effect.usage_total = Some(usage);
                effect.error_code = Some(error_code(&reason, class));
                effect.error = Some(
                    message
                        .filter(|m| !m.trim().is_empty())
                        .unwrap_or_else(|| reason.as_str().to_owned()),
                );
                effect.append = render::failure(&reason, effect.error.as_deref().unwrap_or(""));
            }
            RunEvent::Cancelled { by, reason } => {
                effect.status = Some(TurnStatus::Cancelled);
                effect.append = render::cancellation(&by, &reason);
                // A stopped turn keeps whatever it produced (`plan/08` §1.9).
                effect.answer = Some(String::new());
            }
            _ => {}
        }
        Ok(effect)
    }

    /// The title of a task, from the in-process cache or from `task.created`.
    async fn title(&mut self, conn: &mut PgConnection, task_id: Uuid) -> ProjectionResult<String> {
        if let Some(title) = self.titles.get(&task_id) {
            return Ok(title.clone());
        }
        let title: Option<String> = sqlx::query_scalar(
            "SELECT payload -> 'spec' ->> 'title' FROM core.events \
             WHERE aggregate_type = 'task' AND aggregate_id = $1 AND event_type = 'task.created' \
             LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        let title = title.unwrap_or_else(|| "task".to_owned());
        self.titles.insert(task_id, title.clone());
        Ok(title)
    }
}

#[async_trait]
impl Projection for KohralLedgerProjection {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        HANDLED.contains(&event_type)
    }

    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> ProjectionResult<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let run_id = event.envelope.correlation_id;
        let position = i64::try_from(event.position).unwrap_or(i64::MAX);
        // `FOR UPDATE` inside the runner's transaction: the acceptance path
        // may be inserting a sibling row concurrently.
        let Some(state) = load_state(conn, run_id, position).await? else {
            // Not a Kohral turn (or already terminal / already folded).
            return Ok(());
        };

        let effect = self.effect(event, conn).await?;
        let append = if self.narrative.renders_progress() {
            effect.append.unwrap_or_default()
        } else {
            String::new()
        };

        let mut partial = state.partial_output.clone();
        let mut seq = state.seq;
        if !append.is_empty() {
            partial.push_str(&append);
            seq += 1;
        }
        let terminal = effect.status.is_some_and(TurnStatus::is_terminal);
        if terminal {
            if let Some(answer) = effect.answer.as_deref().filter(|a| !a.trim().is_empty()) {
                partial = reconcile_completed_output(&partial, answer.trim());
            }
            seq += 1;
        }

        let usage = merge_usage(&state.usage, effect.usage_delta, effect.usage_total);

        sqlx::query(
            "UPDATE kohral.runs_ledger \
             SET status = coalesce($2, status), \
                 partial_output = $3, \
                 seq = $4, \
                 usage = $5, \
                 error_code = coalesce($6, error_code), \
                 error = coalesce($7, error), \
                 last_event = $8, \
                 last_position = $9, \
                 updated_at = now() \
             WHERE run_id = $1 AND last_position < $9 \
               AND status NOT IN ('completed', 'failed', 'cancelled')",
        )
        .bind(run_id)
        .bind(effect.status.map(TurnStatus::as_str))
        .bind(&partial)
        .bind(seq)
        .bind(serde_json::to_value(usage).map_err(ProjectionError::Json)?)
        .bind(effect.error_code.as_deref())
        .bind(effect.error.as_deref())
        .bind(effect.last_event)
        .bind(position)
        .execute(&mut *conn)
        .await?;

        if terminal {
            let assistant = crate::ledger::SessionMessage {
                message_id: state.message_id,
                session_id: state.session_id,
                run_id,
                role: "assistant".to_owned(),
                content: partial,
                created_at: event.envelope.occurred_at,
            };
            insert_message(conn, &assistant).await?;
        }
        Ok(())
    }

    async fn reset(&self, pool: &PgPool) -> ProjectionResult<()> {
        // The acceptance row itself is *not* a projection artefact — it is the
        // durable record that Kevin promised Kohral to run this turn, and
        // deleting it would break the idempotency contract. A rebuild
        // therefore rewinds the folded state and replays it.
        sqlx::query(
            "UPDATE kohral.runs_ledger \
             SET status = 'queued', partial_output = '', seq = 0, usage = '{}'::jsonb, \
                 error_code = NULL, error = NULL, last_event = NULL, last_position = 0",
        )
        .execute(pool)
        .await?;
        sqlx::query("DELETE FROM kohral.session_messages WHERE role = 'assistant'")
            .execute(pool)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// What one event does to a row.
#[derive(Debug)]
struct Effect {
    status: Option<TurnStatus>,
    append: Option<String>,
    usage_delta: Option<Usage>,
    usage_total: Option<Usage>,
    error_code: Option<String>,
    error: Option<String>,
    answer: Option<String>,
    last_event: &'static str,
}

impl Effect {
    const fn new(last_event: &'static str) -> Self {
        Self {
            status: None,
            append: None,
            usage_delta: None,
            usage_total: None,
            error_code: None,
            error: None,
            answer: None,
            last_event,
        }
    }
}

/// The mutable part of a ledger row, locked for this transaction.
#[derive(Debug)]
struct LedgerState {
    partial_output: String,
    seq: i64,
    usage: serde_json::Value,
    message_id: String,
    session_id: String,
}

async fn load_state(
    conn: &mut PgConnection,
    run_id: Uuid,
    position: i64,
) -> ProjectionResult<Option<LedgerState>> {
    let row = sqlx::query(
        "SELECT partial_output, seq, usage, message_id, session_id \
         FROM kohral.runs_ledger \
         WHERE run_id = $1 AND last_position < $2 \
           AND status NOT IN ('completed', 'failed', 'cancelled') \
         FOR UPDATE",
    )
    .bind(run_id)
    .bind(position)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok(LedgerState {
            partial_output: row.try_get("partial_output")?,
            seq: row.try_get("seq")?,
            usage: row.try_get("usage")?,
            message_id: row.try_get("message_id")?,
            session_id: row.try_get("session_id")?,
        })
    })
    .transpose()
    .map_err(ProjectionError::Db)
}

async fn insert_message(
    conn: &mut PgConnection,
    message: &crate::ledger::SessionMessage,
) -> ProjectionResult<()> {
    sqlx::query(
        "INSERT INTO kohral.session_messages \
         (message_id, session_id, run_id, role, content, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (message_id) DO UPDATE SET content = EXCLUDED.content",
    )
    .bind(&message.message_id)
    .bind(&message.session_id)
    .bind(message.run_id)
    .bind(&message.role)
    .bind(&message.content)
    .bind(message.created_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Decodes an event payload with the position in the error, like the
/// orchestrator's own projections do.
fn payload<E: DeserializeOwned>(event: &BusEvent) -> ProjectionResult<E> {
    serde_json::from_value(event.envelope.payload.clone()).map_err(|source| {
        ProjectionError::Payload {
            event_type: event.envelope.event_type,
            position: event.position,
            source,
        }
    })
}

/// `question.answered`, decoded structurally (the domain variant is not
/// nameable on its own).
#[derive(Debug, serde::Deserialize)]
struct QuestionAnswered {
    answer: kevin_domain::Answer,
    answered_by: String,
}

async fn question_text(conn: &mut PgConnection, question_id: Uuid) -> ProjectionResult<String> {
    let text: Option<String> = sqlx::query_scalar(
        "SELECT payload ->> 'text' FROM core.events \
         WHERE aggregate_type = 'question' AND aggregate_id = $1 AND event_type = 'question.asked' \
         LIMIT 1",
    )
    .bind(question_id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    Ok(text.unwrap_or_else(|| "an open question".to_owned()))
}

fn task_usage(event: &TaskEvent) -> Option<Usage> {
    match event {
        TaskEvent::Progressed { usage_delta, .. } => Some(*usage_delta),
        _ => None,
    }
}

/// `usage` merge: a delta is added, a cumulative total replaces — and never
/// shrinks the stored value, because Kohral surfaces it to an operator.
fn merge_usage(
    stored: &serde_json::Value,
    delta: Option<Usage>,
    total: Option<Usage>,
) -> serde_json::Value {
    let current: Usage = serde_json::from_value(stored.clone()).unwrap_or_default();
    let merged = match (delta, total) {
        (_, Some(total)) if total.total_tokens() >= current.total_tokens() => total,
        (Some(delta), _) => current + delta,
        _ => current,
    };
    serde_json::json!({
        "input_tokens": merged.input_tokens,
        "output_tokens": merged.output_tokens,
        "cache_read_tokens": merged.cache_read_tokens,
        "cache_write_tokens": merged.cache_write_tokens,
        "cost_usd": merged.cost_usd.map_or(0.0, |cost| {
            cost.to_string().parse::<f64>().unwrap_or(0.0)
        }),
    })
}

/// The stable `error_code` Kohral classifies a failed turn by.
///
/// `RunFailureReason` is already `snake_case` and its known values match
/// Kohral's `^[a-z][a-z0-9_]{1,63}$`; a free-form reason is sanitised, and an
/// unusable one falls back to the failure class.
#[must_use]
pub fn error_code(reason: &RunFailureReason, class: FailureClass) -> String {
    if class == FailureClass::RuntimeRestarted {
        return RUNTIME_RESTARTED.to_owned();
    }
    sanitise(reason.as_str()).unwrap_or_else(|| {
        sanitise(class.as_str()).unwrap_or_else(|| "runtime_chat_failed".to_owned())
    })
}

fn sanitise(value: &str) -> Option<String> {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    let mut chars = trimmed.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase() || trimmed.len() < 2 {
        return None;
    }
    Some(trimmed.chars().take(64).collect())
}

#[cfg(test)]
mod tests {
    use kevin_domain::{FailureClass, RunFailureReason, Usage};

    use super::{Narrative, error_code, merge_usage};

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::ZERO
        }
    }

    #[test]
    fn the_conformance_profile_renders_the_answer_only() {
        let mut config = kevin_config::KevinConfig::default();
        assert_eq!(Narrative::for_config(&config), Narrative::Full);
        config.workers.fake.enabled = true;
        assert_eq!(Narrative::for_config(&config), Narrative::AnswerOnly);
    }

    #[test]
    fn a_restart_always_maps_to_the_code_kohral_expects() {
        assert_eq!(
            error_code(
                &RunFailureReason::RuntimeRestarted,
                FailureClass::RuntimeRestarted
            ),
            "runtime_restarted"
        );
        assert_eq!(
            error_code(
                &RunFailureReason::Other("anything".to_owned()),
                FailureClass::RuntimeRestarted
            ),
            "runtime_restarted",
            "the class alone is enough: the contract pins this code"
        );
    }

    #[test]
    fn known_reasons_become_their_own_code() {
        assert_eq!(
            error_code(&RunFailureReason::BudgetExhausted, FailureClass::Budget),
            "budget_exhausted"
        );
        assert_eq!(
            error_code(&RunFailureReason::TaskFailed, FailureClass::Permanent),
            "task_failed"
        );
        assert_eq!(
            error_code(
                &RunFailureReason::UnansweredQuestion,
                FailureClass::Permanent
            ),
            "unanswered_question"
        );
    }

    #[test]
    fn a_free_form_reason_is_sanitised_into_a_legal_code() {
        let pattern = regex::Regex::new("^[a-z][a-z0-9_]{1,63}$").expect("regex");
        for reason in ["Worker Failed!", "understanding_failed", "??", "x"] {
            let code = error_code(
                &RunFailureReason::Other(reason.to_owned()),
                FailureClass::Permanent,
            );
            assert!(pattern.is_match(&code), "{reason:?} → {code:?}");
        }
        assert_eq!(
            error_code(
                &RunFailureReason::Other("??".to_owned()),
                FailureClass::Permanent
            ),
            "permanent",
            "an unusable reason falls back to the failure class"
        );
    }

    #[test]
    fn usage_deltas_add_and_totals_replace() {
        let empty = serde_json::json!({});
        let after_delta = merge_usage(&empty, Some(usage(10, 5)), None);
        assert_eq!(after_delta["input_tokens"], 10);
        assert_eq!(after_delta["output_tokens"], 5);

        let after_second = merge_usage(&after_delta, Some(usage(1, 1)), None);
        assert_eq!(after_second["input_tokens"], 11);

        let after_total = merge_usage(&after_second, None, Some(usage(100, 50)));
        assert_eq!(after_total["input_tokens"], 100);

        let stale_total = merge_usage(&after_total, None, Some(usage(1, 1)));
        assert_eq!(
            stale_total["input_tokens"], 100,
            "a smaller cumulative total never shrinks what Kohral already saw"
        );
    }
}
