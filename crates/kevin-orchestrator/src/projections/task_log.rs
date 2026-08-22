//! `orch.task_log` — the append-only per-attempt transcript
//! (`TaskLogLineDto`, `plan/01-architecture.md` §Worker streams are not domain
//! events).
//!
//! Two writers share the table:
//!
//! - [`TaskLog`] is the API the task runner (WS-08) uses to append the worker
//!   stream (`assistant`, `tool_call`, `tool_result`, `usage`, …);
//! - [`TaskLogProjection`] appends one `system` line per task lifecycle event,
//!   so a transcript reads as a whole story even without a worker stream.
//!
//! `seq` is assigned per `(task_id, attempt)` as `max(seq) + 1` under a
//! transaction-scoped advisory lock on that pair, so appends are strictly
//! monotonic and gap-free whoever writes them and however many writers run in
//! parallel. Projection lines carry `source_event_id`, whose unique index makes
//! a replay a no-op (the seq is not consumed).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kevin_bus::BusEvent;
use kevin_domain::ids::{AttemptId, EventId, RunId, TaskId};
use kevin_domain::task::TaskEvent;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::helpers::{at, attempt_no, payload, run_id};
use super::{Projection, Result};

/// Projection name / checkpoint key.
pub(crate) const NAME: &str = "task_log";

/// `kind` of the lines the projection writes.
pub const SYSTEM_KIND: &str = "system";

/// Attempt number used by lines that belong to the task itself
/// (`task.created`, `task.routed`, `task.retried`, …).
pub const TASK_LEVEL_ATTEMPT: i32 = 0;

/// One line to append to `orch.task_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTaskLogLine {
    /// Task the line belongs to.
    pub task_id: TaskId,
    /// Attempt number (`0` for task-level lines).
    pub attempt: i32,
    /// When the line was produced.
    pub at: DateTime<Utc>,
    /// `assistant`, `tool_call`, `tool_result`, `usage`, `system`, …
    pub kind: String,
    /// The line itself.
    pub payload: Value,
    /// Owning run, when known.
    pub run_id: Option<RunId>,
    /// Attempt id, when known.
    pub attempt_id: Option<AttemptId>,
    /// Domain event that produced the line (projection lines only); makes the
    /// append idempotent.
    pub source_event_id: Option<EventId>,
}

impl NewTaskLogLine {
    /// A worker line for `task_id`/`attempt`.
    #[must_use]
    pub fn new(task_id: TaskId, attempt: i32, kind: impl Into<String>, payload: Value) -> Self {
        Self {
            task_id,
            attempt,
            at: Utc::now(),
            kind: kind.into(),
            payload,
            run_id: None,
            attempt_id: None,
            source_event_id: None,
        }
    }

    /// Sets the timestamp.
    #[must_use]
    pub const fn at(mut self, at: DateTime<Utc>) -> Self {
        self.at = at;
        self
    }

    /// Sets the run.
    #[must_use]
    pub const fn run(mut self, run_id: RunId) -> Self {
        self.run_id = Some(run_id);
        self
    }

    /// Sets the attempt id.
    #[must_use]
    pub const fn attempt_id(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = Some(attempt_id);
        self
    }
}

/// Append/prune access to `orch.task_log`.
#[derive(Debug, Clone)]
pub struct TaskLog {
    pool: PgPool,
}

impl TaskLog {
    /// Wraps a pool.
    #[must_use]
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Appends one line; returns its `seq`.
    pub async fn append(&self, line: &NewTaskLogLine) -> Result<u64> {
        let seqs = self.append_all(std::slice::from_ref(line)).await?;
        Ok(seqs.first().copied().unwrap_or_default())
    }

    /// Appends `lines` in one transaction, in order; returns their `seq`s.
    pub async fn append_all(&self, lines: &[NewTaskLogLine]) -> Result<Vec<u64>> {
        let mut tx = self.pool.begin().await?;
        let mut seqs = Vec::with_capacity(lines.len());
        for line in lines {
            seqs.push(append_line(&mut tx, line).await?.unwrap_or_default());
        }
        tx.commit().await?;
        Ok(seqs)
    }

    /// Deletes lines older than `days` (`retention.task_log_days`, the
    /// cut-off `kevin db prune` applies); returns how many rows went.
    pub async fn prune_older_than_days(&self, days: u32) -> Result<u64> {
        self.prune(Utc::now() - chrono::TimeDelta::days(i64::from(days)))
            .await
    }

    /// Deletes lines older than `cutoff`; returns how many rows went.
    pub async fn prune(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        let done = sqlx::query("DELETE FROM orch.task_log WHERE at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// Appends one line on an open transaction. Returns `None` when the line was
/// already there (same `source_event_id`).
pub(crate) async fn append_line(
    conn: &mut PgConnection,
    line: &NewTaskLogLine,
) -> Result<Option<u64>> {
    let task = line.task_id.as_uuid();
    // Serialise seq allocation per (task, attempt); the lock is released with
    // the surrounding transaction.
    sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
        .bind(lock_key(task))
        .bind(line.attempt)
        .execute(&mut *conn)
        .await?;
    // `plan/09-security.md` §Redaction: `orch.task_log` holds raw worker
    // output (`assistant`, `tool_call`, `tool_result` lines) and feeds the API
    // and the SSE stream, so it is a redaction sink like the event store. A
    // worker that runs `cat .env` must not leave the credential in the table.
    let mut payload = line.payload.clone();
    kevin_telemetry::redact::Redactor::global().redact_value(&mut payload);
    let seq: Option<i64> = sqlx::query_scalar(
        "INSERT INTO orch.task_log (
             task_id, attempt, seq, at, kind, payload, run_id, attempt_id, source_event_id)
         SELECT $1, $2, coalesce(max(seq), 0) + 1, $3, $4, $5, $6, $7, $8
         FROM orch.task_log WHERE task_id = $1 AND attempt = $2
         ON CONFLICT (source_event_id) WHERE source_event_id IS NOT NULL DO NOTHING
         RETURNING seq",
    )
    .bind(task)
    .bind(line.attempt)
    .bind(line.at)
    .bind(&line.kind)
    .bind(&payload)
    .bind(line.run_id.map(|r| r.as_uuid()))
    .bind(line.attempt_id.map(|a| a.as_uuid()))
    .bind(line.source_event_id.map(|e| e.as_uuid()))
    .fetch_optional(&mut *conn)
    .await?;
    Ok(seq.map(|s| u64::try_from(s).unwrap_or_default()))
}

/// Advisory-lock key derived from the task id (stable, no hashing extension).
fn lock_key(task: Uuid) -> i32 {
    let bytes = task.as_bytes();
    i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Writes one `system` line per `task.*` event.
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskLogProjection;

impl TaskLogProjection {
    /// A new projection.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Projection for TaskLogProjection {
    fn name(&self) -> &'static str {
        NAME
    }

    fn handles(&self, event_type: &str) -> bool {
        event_type.starts_with("task.")
    }

    async fn reset(&self, pool: &PgPool) -> Result<()> {
        // Worker lines are not replayable, so a rebuild only drops the lines
        // this projection wrote.
        sqlx::query("DELETE FROM orch.task_log WHERE source_event_id IS NOT NULL")
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()> {
        if !self.handles(event.envelope.event_type) {
            return Ok(());
        }
        let task = TaskId::from_uuid(event.envelope.aggregate_id);
        let task_event = payload::<TaskEvent>(event)?;
        let attempt_id = attempt_of(&task_event);
        let attempt = match attempt_id {
            Some(id) => attempt_no(conn, task.as_uuid(), id.as_uuid())
                .await?
                .unwrap_or(TASK_LEVEL_ATTEMPT),
            None => TASK_LEVEL_ATTEMPT,
        };
        let mut line = NewTaskLogLine {
            task_id: task,
            attempt,
            at: at(event),
            kind: SYSTEM_KIND.to_owned(),
            payload: event.envelope.payload.clone(),
            run_id: Some(RunId::from_uuid(run_id(event))),
            attempt_id,
            source_event_id: Some(event.envelope.event_id),
        };
        if let TaskEvent::AttemptStarted { attempt_no: no, .. } = task_event {
            line.attempt = i32::from(no);
        }
        append_line(conn, &line).await?;
        Ok(())
    }
}

/// The attempt a task event belongs to, when it names one.
const fn attempt_of(event: &TaskEvent) -> Option<AttemptId> {
    match event {
        TaskEvent::AttemptStarted { attempt_id, .. }
        | TaskEvent::Progressed { attempt_id, .. }
        | TaskEvent::InputRequested { attempt_id, .. }
        | TaskEvent::InputProvided { attempt_id, .. }
        | TaskEvent::AttemptSucceeded { attempt_id, .. }
        | TaskEvent::AttemptFailed { attempt_id, .. } => Some(*attempt_id),
        _ => None,
    }
}
