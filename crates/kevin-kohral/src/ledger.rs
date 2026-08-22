//! `kohral.runs_ledger` — the durable turn status (`plan/08-kohral-runtime.md`
//! §1.3 and §2).
//!
//! Kohral polls; it never holds a connection open and it never trusts
//! in-memory state. Everything `GET /v1/runs/{id}` answers therefore comes from
//! this table, which survives a crash, and which obeys Kohral's turn
//! invariants (`kohral docs/10-conversations.md` §Turn invariants):
//!
//! - `partial_output` is **append-only** — a rewritten prefix is a
//!   `runtime_protocol_error` on Kohral's side;
//! - `seq` is **monotonic** — `+1` per append and `+1` on the terminal
//!   transition;
//! - a terminal row never changes again, so a restart cannot un-fail a turn.
//!
//! The row is written synchronously at acceptance and then only ever advanced
//! by [`crate::projection::KohralLedgerProjection`], which folds the run's
//! events with a `last_position` guard so a projection rebuild cannot append
//! the same narrative twice.

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgExecutor, PgPool, Row};
use uuid::Uuid;

use crate::error::{KohralError, KohralErrorCode, KohralResult};

/// The status vocabulary `HermesRuntimeStrategy::turnStatus()` normalises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// Accepted, not started.
    Queued,
    /// Executing.
    Running,
    /// A stop was requested; the run has not terminalised yet.
    Stopping,
    /// Finished with an answer.
    Completed,
    /// Finished without one.
    Failed,
    /// Interrupted (Kohral shows `interrupted`).
    Cancelled,
}

impl TurnStatus {
    /// Every status, in lifecycle order.
    pub const ALL: [TurnStatus; 6] = [
        TurnStatus::Queued,
        TurnStatus::Running,
        TurnStatus::Stopping,
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Cancelled,
    ];

    /// The wire string (also the `status` check constraint).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TurnStatus::Queued => "queued",
            TurnStatus::Running => "running",
            TurnStatus::Stopping => "stopping",
            TurnStatus::Completed => "completed",
            TurnStatus::Failed => "failed",
            TurnStatus::Cancelled => "cancelled",
        }
    }

    /// `true` for `completed`, `failed` and `cancelled`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Cancelled
        )
    }

    /// Parses a database value; anything unknown reads as `failed` rather than
    /// panicking, because a status a Kohral worker cannot map is a protocol
    /// error on its side.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        TurnStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .unwrap_or(TurnStatus::Failed)
    }
}

/// The failure code Kohral expects after a restart.
pub const RUNTIME_RESTARTED: &str = "runtime_restarted";

/// Diagnostic text stored with a [`RUNTIME_RESTARTED`] failure.
pub const RUNTIME_RESTARTED_MESSAGE: &str = "Runtime restarted after accepting this run.";

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// One row of `kohral.runs_ledger`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    /// Kohral's turn id.
    pub idempotency_key: String,
    /// Canonical hash of the accepted request.
    pub request_hash: String,
    /// Kevin's run id, returned to Kohral as `run_id`.
    pub run_id: Uuid,
    /// Kohral conversation id.
    pub session_id: String,
    /// `X-Hermes-Session-Key`.
    pub session_key: Option<String>,
    /// The `model` field of the request, verbatim.
    pub model: Option<String>,
    /// Durable status.
    pub status: TurnStatus,
    /// Append-only progress narrative + final answer.
    pub partial_output: String,
    /// Monotonic sequence.
    pub seq: i64,
    /// Stable id of the assistant message this turn produces.
    pub message_id: String,
    /// Accumulated usage.
    pub usage: Value,
    /// Stable failure code (`^[a-z][a-z0-9_]{1,63}$`), only when failed.
    pub error_code: Option<String>,
    /// Diagnostic text, only when failed.
    pub error: Option<String>,
    /// `event_type` of the last event folded into this row.
    pub last_event: Option<String>,
    /// When the turn was accepted.
    pub created_at: DateTime<Utc>,
    /// When the row last changed.
    pub updated_at: DateTime<Utc>,
}

impl LedgerRow {
    /// The `GET /v1/runs/{run_id}` body (`plan/08` §1.3).
    ///
    /// `output` appears **only** on a completed run and is exactly
    /// `partial_output`, which is what `contract.py` compares against.
    #[must_use]
    pub fn status_object(&self) -> Value {
        let mut object = json!({
            "object": "kevin.run",
            "run_id": self.run_id,
            "status": self.status.as_str(),
            "partial_output": self.partial_output,
            "seq": self.seq,
            "message_id": self.message_id,
            "usage": self.usage,
            "session_id": self.session_id,
            "created_at": epoch(self.created_at),
            "updated_at": epoch(self.updated_at),
        });
        let map = object.as_object_mut().expect("object");
        if self.status == TurnStatus::Completed {
            map.insert(
                "output".to_owned(),
                Value::String(self.partial_output.clone()),
            );
        }
        if let Some(model) = &self.model {
            map.insert("model".to_owned(), Value::String(model.clone()));
        }
        if let Some(last_event) = &self.last_event {
            map.insert("last_event".to_owned(), Value::String(last_event.clone()));
        }
        if self.status == TurnStatus::Failed {
            if let Some(code) = &self.error_code {
                map.insert("error_code".to_owned(), Value::String(code.clone()));
            }
            if let Some(error) = &self.error {
                map.insert("error".to_owned(), Value::String(error.clone()));
            }
        }
        object
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            idempotency_key: row.try_get("idempotency_key")?,
            request_hash: row.try_get("request_hash")?,
            run_id: row.try_get("run_id")?,
            session_id: row.try_get("session_id")?,
            session_key: row.try_get("session_key")?,
            model: row.try_get("model")?,
            status: TurnStatus::parse(&row.try_get::<String, _>("status")?),
            partial_output: row.try_get("partial_output")?,
            seq: row.try_get("seq")?,
            message_id: row.try_get("message_id")?,
            usage: row.try_get("usage")?,
            error_code: row.try_get("error_code")?,
            error: row.try_get("error")?,
            last_event: row.try_get("last_event")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// A turn Kevin is about to accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTurn {
    /// `Idempotency-Key`.
    pub idempotency_key: String,
    /// [`crate::hash::canonical_request_hash`] of the request.
    pub request_hash: String,
    /// The canonical request envelope, stored for diagnosis.
    pub request_json: Value,
    /// The run id Kevin allocated.
    pub run_id: Uuid,
    /// Kohral conversation id.
    pub session_id: String,
    /// `X-Hermes-Session-Key`.
    pub session_key: Option<String>,
    /// The requested model, verbatim.
    pub model: Option<String>,
    /// Stable assistant message id.
    pub message_id: String,
}

/// What [`RunsLedger::accept`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// The key is new; the row was inserted and the caller must now start the
    /// run (`202`).
    Fresh(Box<LedgerRow>),
    /// The key was already used with the *same* request (`200`).
    Replay(Box<LedgerRow>),
}

impl Accepted {
    /// The row, whichever branch this is.
    #[must_use]
    pub fn row(&self) -> &LedgerRow {
        match self {
            Accepted::Fresh(row) | Accepted::Replay(row) => row,
        }
    }
}

/// One message of a Kohral session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMessage {
    /// Stable id (`umsg_<run_id>` / the run's `message_id`).
    pub message_id: String,
    /// The conversation.
    pub session_id: String,
    /// The turn that produced it.
    pub run_id: Uuid,
    /// `user` or `assistant`.
    pub role: String,
    /// The text.
    pub content: String,
    /// When it was recorded.
    pub created_at: DateTime<Utc>,
}

impl SessionMessage {
    /// The wire shape Kohral reconciles against.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.message_id,
            "message_id": self.message_id,
            "role": self.role,
            "content": self.content,
            "created_at": epoch(self.created_at),
            "run_id": self.run_id,
        })
    }
}

/// Summary of one Kohral conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The conversation id.
    pub session_id: String,
    /// First turn.
    pub created_at: DateTime<Utc>,
    /// Last turn.
    pub updated_at: DateTime<Utc>,
    /// Messages recorded so far.
    pub message_count: i64,
    /// Runs in the session, oldest first.
    pub runs: Vec<Uuid>,
}

impl SessionSummary {
    /// The wire shape (`id` **and** `session_id`: Kohral reads either).
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.session_id,
            "session_id": self.session_id,
            "title": self.session_id,
            "created_at": epoch(self.created_at),
            "updated_at": epoch(self.updated_at),
            "message_count": self.message_count,
            "runs": self.runs,
        })
    }
}

// ---------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------

/// Query and maintenance API over `kohral.runs_ledger`.
#[derive(Debug, Clone)]
pub struct RunsLedger {
    pool: PgPool,
}

impl RunsLedger {
    /// A ledger over `pool`.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool, for callers that need to run in the same database.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Inserts the acceptance row, or reports the existing one.
    ///
    /// The insert is `ON CONFLICT DO NOTHING`, so two concurrent submissions of
    /// the same key cannot both be [`Accepted::Fresh`]: exactly one starts the
    /// run and the other polls it. A key that exists with a **different**
    /// `request_hash` is an [`KohralErrorCode::IdempotencyConflict`].
    pub async fn accept(&self, turn: &NewTurn) -> KohralResult<Accepted> {
        let inserted = sqlx::query(
            "INSERT INTO kohral.runs_ledger \
             (idempotency_key, request_hash, request_json, run_id, session_id, session_key, \
              model, status, message_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'queued', $8) \
             ON CONFLICT (idempotency_key) DO NOTHING \
             RETURNING idempotency_key, request_hash, run_id, session_id, session_key, model, \
                       status, partial_output, seq, message_id, usage, error_code, error, \
                       last_event, created_at, updated_at",
        )
        .bind(&turn.idempotency_key)
        .bind(&turn.request_hash)
        .bind(&turn.request_json)
        .bind(turn.run_id)
        .bind(&turn.session_id)
        .bind(&turn.session_key)
        .bind(&turn.model)
        .bind(&turn.message_id)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = inserted {
            return Ok(Accepted::Fresh(Box::new(LedgerRow::from_row(&row)?)));
        }

        let existing = self.by_key(&turn.idempotency_key).await?.ok_or_else(|| {
            KohralError::new(KohralErrorCode::InternalError, "ledger row vanished")
        })?;
        if existing.request_hash != turn.request_hash {
            return Err(KohralError::new(
                KohralErrorCode::IdempotencyConflict,
                "this Idempotency-Key was already used with a different request",
            ));
        }
        Ok(Accepted::Replay(Box::new(existing)))
    }

    /// Removes an acceptance row. Used only when starting the run failed
    /// *before* Kevin answered, so the same key may be retried.
    pub async fn forget(&self, idempotency_key: &str) -> KohralResult<()> {
        sqlx::query("DELETE FROM kohral.runs_ledger WHERE idempotency_key = $1")
            .bind(idempotency_key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The row for an `Idempotency-Key`.
    pub async fn by_key(&self, idempotency_key: &str) -> KohralResult<Option<LedgerRow>> {
        let row = sqlx::query(
            "SELECT idempotency_key, request_hash, run_id, session_id, session_key, model, \
                    status, partial_output, seq, message_id, usage, error_code, error, \
                    last_event, created_at, updated_at \
             FROM kohral.runs_ledger WHERE idempotency_key = $1",
        )
        .bind(idempotency_key)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(LedgerRow::from_row)
            .transpose()
            .map_err(Into::into)
    }

    /// The row for a run id.
    pub async fn by_run(&self, run_id: Uuid) -> KohralResult<Option<LedgerRow>> {
        let row = sqlx::query(
            "SELECT idempotency_key, request_hash, run_id, session_id, session_key, model, \
                    status, partial_output, seq, message_id, usage, error_code, error, \
                    last_event, created_at, updated_at \
             FROM kohral.runs_ledger WHERE run_id = $1",
        )
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(LedgerRow::from_row)
            .transpose()
            .map_err(Into::into)
    }

    /// Marks a non-terminal run `stopping` (idempotent). Returns the row as it
    /// is now, terminal rows untouched.
    pub async fn mark_stopping(&self, run_id: Uuid) -> KohralResult<Option<LedgerRow>> {
        sqlx::query(
            "UPDATE kohral.runs_ledger \
             SET status = 'stopping', last_event = 'run.stopping', updated_at = now() \
             WHERE run_id = $1 AND status IN ('queued', 'running')",
        )
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        self.by_run(run_id).await
    }

    /// Number of runs that are still non-terminal (`activeWork` in the drain
    /// payload, `active_runs` in `/health/detailed`).
    pub async fn active_runs(&self) -> KohralResult<i64> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM kohral.runs_ledger \
             WHERE status IN ('queued', 'running', 'stopping')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Terminalises every non-terminal row as `failed / runtime_restarted`
    /// (`plan/08` §1.9). Partial output is preserved and `seq` still
    /// increments, so a Kohral worker polling across the restart sees a
    /// forward-only transition. Returns the affected run ids.
    ///
    /// Kevin **never** replays accepted work: the sweep is the whole recovery
    /// story, and `run_automatic_replay` is advertised as `false`.
    pub async fn sweep_runtime_restarted(&self) -> KohralResult<Vec<Uuid>> {
        let rows = sqlx::query(
            "UPDATE kohral.runs_ledger \
             SET status = 'failed', error_code = $1, error = $2, last_event = 'run.failed', \
                 seq = seq + 1, updated_at = now() \
             WHERE status IN ('queued', 'running', 'stopping') \
             RETURNING run_id",
        )
        .bind(RUNTIME_RESTARTED)
        .bind(RUNTIME_RESTARTED_MESSAGE)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| row.try_get::<Uuid, _>("run_id"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Records the turn's user message (called at acceptance).
    pub async fn record_user_message(&self, message: &SessionMessage) -> KohralResult<()> {
        insert_message(&self.pool, message).await
    }

    /// Sessions, newest activity first.
    pub async fn sessions(&self, limit: i64, offset: i64) -> KohralResult<Vec<SessionSummary>> {
        let rows = sqlx::query(
            "SELECT l.session_id, \
                    min(l.created_at) AS created_at, \
                    max(l.updated_at) AS updated_at, \
                    array_agg(l.run_id ORDER BY l.created_at) AS runs, \
                    coalesce(( \
                        SELECT count(*) FROM kohral.session_messages m \
                        WHERE m.session_id = l.session_id), 0) AS message_count \
             FROM kohral.runs_ledger l \
             GROUP BY l.session_id \
             ORDER BY max(l.updated_at) DESC \
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(session_summary).collect()
    }

    /// One session, or `None`.
    pub async fn session(&self, session_id: &str) -> KohralResult<Option<SessionSummary>> {
        let row = sqlx::query(
            "SELECT l.session_id, \
                    min(l.created_at) AS created_at, \
                    max(l.updated_at) AS updated_at, \
                    array_agg(l.run_id ORDER BY l.created_at) AS runs, \
                    coalesce(( \
                        SELECT count(*) FROM kohral.session_messages m \
                        WHERE m.session_id = l.session_id), 0) AS message_count \
             FROM kohral.runs_ledger l \
             WHERE l.session_id = $1 \
             GROUP BY l.session_id",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(session_summary).transpose()
    }

    /// Messages of a session, oldest first.
    pub async fn messages(&self, session_id: &str) -> KohralResult<Vec<SessionMessage>> {
        let rows = sqlx::query(
            "SELECT message_id, session_id, run_id, role, content, created_at \
             FROM kohral.session_messages WHERE session_id = $1 \
             ORDER BY created_at, message_id",
        )
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(SessionMessage {
                    message_id: row.try_get("message_id")?,
                    session_id: row.try_get("session_id")?,
                    run_id: row.try_get("run_id")?,
                    role: row.try_get("role")?,
                    content: row.try_get("content")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(Into::into)
    }
}

fn session_summary(row: &sqlx::postgres::PgRow) -> KohralResult<SessionSummary> {
    Ok(SessionSummary {
        session_id: row.try_get("session_id").map_err(KohralError::from)?,
        created_at: row.try_get("created_at").map_err(KohralError::from)?,
        updated_at: row.try_get("updated_at").map_err(KohralError::from)?,
        message_count: row.try_get("message_count").map_err(KohralError::from)?,
        runs: row.try_get("runs").map_err(KohralError::from)?,
    })
}

/// Upserts a session message; the id is stable so a re-poll never duplicates.
pub(crate) async fn insert_message<'e, E: PgExecutor<'e>>(
    executor: E,
    message: &SessionMessage,
) -> KohralResult<()> {
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
    .execute(executor)
    .await?;
    Ok(())
}

/// Epoch seconds with sub-second precision, the shape Hermes uses.
#[must_use]
pub fn epoch(at: DateTime<Utc>) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let seconds = at.timestamp() as f64;
    #[allow(clippy::cast_lossless)]
    let nanos = f64::from(at.timestamp_subsec_nanos());
    seconds + nanos / 1e9
}

// ---------------------------------------------------------------------------
// Output reconciliation
// ---------------------------------------------------------------------------

/// Port of Hermes' `reconcile_completed_output`: keep everything already
/// streamed to Kohral and append only the part of the terminal answer that has
/// not been seen, so the prefix a Kohral worker already stored is never
/// rewritten.
#[must_use]
pub fn reconcile_completed_output(checkpoint: &str, terminal: &str) -> String {
    if checkpoint.is_empty() {
        return terminal.to_owned();
    }
    if terminal.is_empty() || checkpoint.ends_with(terminal) {
        return checkpoint.to_owned();
    }
    if terminal.starts_with(checkpoint) {
        return terminal.to_owned();
    }

    // KMP: how much of `terminal` is already a suffix of `checkpoint`?
    let terminal: Vec<char> = terminal.chars().collect();
    let mut failure = vec![0usize; terminal.len()];
    let mut matched = 0usize;
    for index in 1..terminal.len() {
        while matched > 0 && terminal[index] != terminal[matched] {
            matched = failure[matched - 1];
        }
        if terminal[index] == terminal[matched] {
            matched += 1;
        }
        failure[index] = matched;
    }

    let checkpoint_chars: Vec<char> = checkpoint.chars().collect();
    let mut matched = 0usize;
    for (index, character) in checkpoint_chars.iter().enumerate() {
        while matched > 0 && *character != terminal[matched] {
            matched = failure[matched - 1];
        }
        if *character == terminal[matched] {
            matched += 1;
        }
        if matched == terminal.len() && index + 1 < checkpoint_chars.len() {
            matched = failure[matched - 1];
        }
    }

    let suffix: String = terminal[matched..].iter().collect();
    if matched > 0 {
        format!("{checkpoint}{suffix}")
    } else {
        format!("{checkpoint}\n\n{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::{LedgerRow, TurnStatus, epoch, reconcile_completed_output};

    fn row(status: TurnStatus) -> LedgerRow {
        LedgerRow {
            idempotency_key: "turn-1".to_owned(),
            request_hash: "0".repeat(64),
            run_id: uuid::Uuid::nil(),
            session_id: "conformance".to_owned(),
            session_key: Some("kohral:conformance".to_owned()),
            model: Some("hermes-agent".to_owned()),
            status,
            partial_output: "kohral-ok".to_owned(),
            seq: 3,
            message_id: "msg_1".to_owned(),
            usage: serde_json::json!({"input_tokens": 1}),
            error_code: Some("runtime_restarted".to_owned()),
            error: Some("boom".to_owned()),
            last_event: Some("run.completed".to_owned()),
            created_at: chrono::Utc
                .timestamp_opt(1_755_770_000, 0)
                .single()
                .expect("ts"),
            updated_at: chrono::Utc
                .timestamp_opt(1_755_770_012, 500_000_000)
                .single()
                .expect("ts"),
        }
    }

    #[test]
    fn a_completed_run_exposes_output_equal_to_partial_output() {
        let object = row(TurnStatus::Completed).status_object();
        assert_eq!(object["status"], "completed");
        assert_eq!(object["output"], "kohral-ok");
        assert_eq!(object["partial_output"], "kohral-ok");
        assert_eq!(object["object"], "kevin.run");
        assert_eq!(object["seq"], 3);
        assert!(
            object.get("error_code").is_none(),
            "a completed run must not carry an error code"
        );
        assert!((object["created_at"].as_f64().expect("f64") - 1_755_770_000.0).abs() < 1e-6);
        assert!((object["updated_at"].as_f64().expect("f64") - 1_755_770_012.5).abs() < 1e-6);
    }

    #[test]
    fn a_running_run_has_no_output_field() {
        let object = row(TurnStatus::Running).status_object();
        assert!(object.get("output").is_none());
        assert_eq!(object["partial_output"], "kohral-ok");
    }

    #[test]
    fn a_failed_run_carries_the_error_code_and_diagnostic() {
        let object = row(TurnStatus::Failed).status_object();
        assert_eq!(object["error_code"], "runtime_restarted");
        assert_eq!(object["error"], "boom");
        assert!(object.get("output").is_none());
    }

    #[test]
    fn statuses_round_trip_and_know_which_are_terminal() {
        for status in TurnStatus::ALL {
            assert_eq!(TurnStatus::parse(status.as_str()), status);
        }
        assert!(!TurnStatus::Queued.is_terminal());
        assert!(!TurnStatus::Running.is_terminal());
        assert!(!TurnStatus::Stopping.is_terminal());
        assert!(TurnStatus::Completed.is_terminal());
        assert!(TurnStatus::Failed.is_terminal());
        assert!(TurnStatus::Cancelled.is_terminal());
    }

    #[test]
    fn reconciliation_never_rewrites_the_streamed_prefix() {
        assert_eq!(reconcile_completed_output("", "final"), "final");
        assert_eq!(reconcile_completed_output("streamed", ""), "streamed");
        // The terminal answer extends what was streamed.
        assert_eq!(reconcile_completed_output("abc", "abcdef"), "abcdef");
        // The terminal answer is the tail of what was streamed.
        assert_eq!(reconcile_completed_output("xxabc", "abc"), "xxabc");
        // Partial overlap: only the unseen suffix is appended.
        assert_eq!(
            reconcile_completed_output("hello wor", "world"),
            "hello world"
        );
        // No overlap at all: a blank line separates the two.
        assert_eq!(
            reconcile_completed_output("progress", "answer"),
            "progress\n\nanswer"
        );
    }

    #[test]
    fn reconciliation_is_append_only_for_every_pair_it_is_given() {
        for (checkpoint, terminal) in [
            ("", "a"),
            ("a", "b"),
            ("### Plan\n", "### Plan\ndone"),
            ("one two", "two three"),
        ] {
            let result = reconcile_completed_output(checkpoint, terminal);
            assert!(
                result.starts_with(checkpoint),
                "{result:?} must keep the prefix {checkpoint:?}"
            );
            assert!(result.len() >= checkpoint.len());
        }
    }

    #[test]
    fn epoch_seconds_keep_sub_second_precision() {
        let at = chrono::Utc
            .timestamp_opt(10, 250_000_000)
            .single()
            .expect("ts");
        assert!((epoch(at) - 10.25).abs() < 1e-9);
    }
}
