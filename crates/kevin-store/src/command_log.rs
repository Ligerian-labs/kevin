//! Processed-command log (`core.processed_commands`): idempotent command replay.
//!
//! Every command carries a `CommandId` (uuid v7). An application service calls
//! [`CommandLog::begin`] first: a [`Begun::Replayed`] answer carries the result
//! recorded by the first execution and **must** be returned to the caller
//! without re-executing; [`Begun::Fresh`] hands out an [`InFlightCommand`]
//! whose [`InFlightCommand::complete`] records the result.
//!
//! The log is crash-safe by construction: nothing is written at `begin`, so a
//! process dying between `begin` and `complete` leaves no trace and a retry
//! simply executes again (the event store's optimistic concurrency check then
//! rejects a duplicate append). Two *concurrent* executions of the same
//! command race at `complete`: the first result wins and the second gets
//! [`CompleteOutcome::AlreadyRecorded`] with the winner's result.

use kevin_domain::CommandId;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::StoreError;

/// Access to `core.processed_commands`.
#[derive(Debug, Clone)]
pub struct CommandLog {
    pool: PgPool,
}

/// Answer of [`CommandLog::begin`].
#[derive(Debug)]
pub enum Begun {
    /// The command was never completed: execute it, then call
    /// [`InFlightCommand::complete`].
    Fresh(InFlightCommand),
    /// The command was already completed; this is the recorded result.
    Replayed(Value),
}

/// A command that is being executed for the first time.
#[derive(Debug)]
pub struct InFlightCommand {
    pool: PgPool,
    command_id: CommandId,
}

/// Answer of [`InFlightCommand::complete`] / [`CommandLog::complete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteOutcome {
    /// `result` is now the recorded result of the command.
    Recorded,
    /// A concurrent execution recorded first; this is its result.
    AlreadyRecorded(Value),
}

impl CommandLog {
    /// Wraps a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Looks `command_id` up: replayed result if already completed, fresh otherwise.
    pub async fn begin(&self, command_id: CommandId) -> Result<Begun, StoreError> {
        match self.result_of(command_id).await? {
            Some(result) => Ok(Begun::Replayed(result)),
            None => Ok(Begun::Fresh(InFlightCommand {
                pool: self.pool.clone(),
                command_id,
            })),
        }
    }

    /// Records `result` for `command_id` unless a result already exists.
    pub async fn complete(
        &self,
        command_id: CommandId,
        result: &Value,
    ) -> Result<CompleteOutcome, StoreError> {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO core.processed_commands (command_id, result) VALUES ($1, $2) \
             ON CONFLICT (command_id) DO NOTHING RETURNING command_id",
        )
        .bind(command_id.as_uuid())
        .bind(result)
        .fetch_optional(&self.pool)
        .await?;
        if inserted.is_some() {
            return Ok(CompleteOutcome::Recorded);
        }
        let existing = self
            .result_of(command_id)
            .await?
            .ok_or_else(|| StoreError::Corrupt {
                table: "core.processed_commands",
                message: format!("{command_id} conflicted on insert but has no row"),
            })?;
        Ok(CompleteOutcome::AlreadyRecorded(existing))
    }

    /// The recorded result of `command_id`, if any.
    pub async fn result_of(&self, command_id: CommandId) -> Result<Option<Value>, StoreError> {
        let result: Option<Value> =
            sqlx::query_scalar("SELECT result FROM core.processed_commands WHERE command_id = $1")
                .bind(command_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(result)
    }
}

impl InFlightCommand {
    /// The command being executed.
    #[must_use]
    pub const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Records `result`; see [`CompleteOutcome`].
    pub async fn complete(self, result: &Value) -> Result<CompleteOutcome, StoreError> {
        CommandLog::new(self.pool)
            .complete(self.command_id, result)
            .await
    }
}
