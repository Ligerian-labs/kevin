//! Projections and read models (`plan/02-domain-model.md` §Read models,
//! `plan/01-architecture.md` §Read path). Schema `orch.*`, migration
//! `0002_orch.sql`. Implemented by WS-11.
//!
//! # Shape
//!
//! - [`Projection`] — one read model. It is fed `BusEvent`s (envelope + global
//!   position) and writes through a `PgConnection` that belongs to the
//!   runner's transaction, so the read-model rows and the checkpoint move
//!   together. Every write is an idempotent upsert keyed by the aggregate
//!   version (or, for append-only tables, by the source event id), which makes
//!   a replay exactly-once *in effect*: applying the same event twice leaves
//!   the tables byte-identical.
//! - [`ProjectionRunner`] — drives one projection: catches up from
//!   `core.projection_checkpoints` through the event store, then follows the
//!   bus; a [`kevin_bus::BusMessage::Lagged`] report simply sends it back to
//!   the store. It exposes `kevin_projection_lag_events` /
//!   `kevin_projection_apply_duration_seconds`, supports
//!   [`ProjectionRunner::rebuild`] (reset + replay from position 0) and stops
//!   on a `CancellationToken`.
//! - [`ReadModels`] — the typed query API the API and the CLI use; it never
//!   touches `core.events`.
//!
//! # Tables
//!
//! | Projection | Table | Source events | DTO (`plan/07`) |
//! |---|---|---|---|
//! | [`RunOverview`] | `orch.run_overview` | `run.*` | `RunDto`, `RunSummaryDto` |
//! | [`TaskBoard`] | `orch.task_board` | `task.*` | `TaskDto`, `AttemptDto` |
//! | [`QuestionInbox`] | `orch.question_inbox` | `question.*` | `QuestionDto` |
//! | [`CostLedger`] | `orch.cost_ledger` | `task.attempt_*` | `CostReportDto` |
//! | [`TaskLogProjection`] | `orch.task_log` | `task.*` (+ worker lines from the task runner) | `TaskLogLineDto` |
//! | [`Artifacts`] | `orch.artifacts` | `task.attempt_succeeded`, `run.integrated` | `ArtifactDto` |

mod artifacts;
mod cost_ledger;
mod query;
mod question_inbox;
mod run_overview;
mod runner;
mod task_board;
mod task_log;

use async_trait::async_trait;
use kevin_bus::BusEvent;
use sqlx::{PgConnection, PgPool};

pub use artifacts::Artifacts;
pub use cost_ledger::CostLedger;
pub use query::{
    ArtifactRow, CostGroupBy, CostLedgerRow, CostQuery, CostReport, CostRow, Cursor, DEFAULT_LIMIT,
    MAX_LIMIT, Page, QuestionInboxRow, QuestionQuery, ReadModels, RunOverviewRow, RunQuery,
    TaskBoardRow, TaskLogQuery, TaskLogRow, TaskQuery, Usd,
};
pub use question_inbox::QuestionInbox;
pub use run_overview::RunOverview;
pub use runner::{DEFAULT_BATCH, ProjectionRunner, RebuildReport, rebuild, rebuild_all, spawn_all};
pub use task_board::TaskBoard;
pub use task_log::{NewTaskLogLine, TaskLog, TaskLogProjection};

/// Name of every projection, in registry order.
pub const NAMES: &[&str] = &[
    run_overview::NAME,
    task_board::NAME,
    question_inbox::NAME,
    cost_ledger::NAME,
    task_log::NAME,
    artifacts::NAME,
];

/// Everything that can go wrong while projecting or querying a read model.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    /// Postgres failed.
    #[error("read model database error: {0}")]
    Db(#[from] sqlx::Error),

    /// The event store failed (catch-up read).
    #[error("event store: {0}")]
    Store(#[from] kevin_store::StoreError),

    /// An event payload did not match the domain schema (unknown event or a
    /// payload that no upcaster could bring to the current shape).
    #[error("event {event_type} at position {position} could not be decoded: {source}")]
    Payload {
        /// The event type from the envelope.
        event_type: &'static str,
        /// Global position of the offending event.
        position: u64,
        /// The serde error.
        #[source]
        source: serde_json::Error,
    },

    /// A read-model column could not be encoded as JSON (never happens for the
    /// domain's own types; a bug if it does).
    #[error("read model JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// `rebuild-projection` was given a name no projection answers to.
    #[error("unknown projection `{name}` (known: {})", NAMES.join(", "))]
    UnknownProjection {
        /// The name that was asked for.
        name: String,
    },

    /// A cursor from a previous page could not be decoded.
    #[error("invalid pagination cursor `{cursor}`")]
    InvalidCursor {
        /// The offending cursor.
        cursor: String,
    },
}

/// Result alias for read-model operations.
pub type Result<T, E = ProjectionError> = std::result::Result<T, E>;

/// One read model, fed events in global position order.
///
/// Implementations must be **idempotent**: applying the same event twice (or
/// applying an older event after a newer one) must not change the result, so
/// that a crash between the write and the checkpoint commit, or a full
/// rebuild, converge to the same tables.
#[async_trait]
pub trait Projection: Send + Sync + std::fmt::Debug {
    /// Stable name; also the key in `core.projection_checkpoints` and the
    /// argument of `kevin db rebuild-projection`.
    fn name(&self) -> &'static str;

    /// Whether this projection cares about `event_type` at all. Used to skip
    /// work (and to keep `handle` free of a big fall-through match).
    fn handles(&self, event_type: &str) -> bool;

    /// Applies `event` on `conn`, which is the runner's open transaction.
    async fn handle(&mut self, event: &BusEvent, conn: &mut PgConnection) -> Result<()>;

    /// Empties the read model (`rebuild`, before replaying from position 0).
    async fn reset(&self, pool: &PgPool) -> Result<()>;
}

/// Every projection of the `orch` schema, freshly built.
#[must_use]
pub fn all() -> Vec<Box<dyn Projection>> {
    vec![
        Box::new(RunOverview::new()),
        Box::new(TaskBoard::new()),
        Box::new(QuestionInbox::new()),
        Box::new(CostLedger::new()),
        Box::new(TaskLogProjection::new()),
        Box::new(Artifacts::new()),
    ]
}

/// The projection called `name`.
pub fn by_name(name: &str) -> Result<Box<dyn Projection>> {
    all()
        .into_iter()
        .find(|p| p.name() == name)
        .ok_or_else(|| ProjectionError::UnknownProjection {
            name: name.to_owned(),
        })
}

// ---------------------------------------------------------------------------
// Shared helpers for the individual projections
// ---------------------------------------------------------------------------

pub(crate) mod helpers {
    use chrono::{DateTime, Utc};
    use kevin_bus::BusEvent;
    use kevin_domain::values::Usage;
    use serde::de::DeserializeOwned;
    use sqlx::PgConnection;
    use uuid::Uuid;

    use super::{ProjectionError, Result};

    /// Decodes the payload of `event` into a typed domain event.
    pub(crate) fn payload<E: DeserializeOwned>(event: &BusEvent) -> Result<E> {
        serde_json::from_value(event.envelope.payload.clone()).map_err(|source| {
            ProjectionError::Payload {
                event_type: event.envelope.event_type,
                position: event.position,
                source,
            }
        })
    }

    /// `aggregate_version` as `i64` (versions are far below `i64::MAX`).
    pub(crate) fn version(event: &BusEvent) -> i64 {
        i64::try_from(event.envelope.aggregate_version).unwrap_or(i64::MAX)
    }

    /// Global position as `i64`.
    pub(crate) fn position(event: &BusEvent) -> i64 {
        i64::try_from(event.position).unwrap_or(i64::MAX)
    }

    /// When the event happened.
    pub(crate) fn at(event: &BusEvent) -> DateTime<Utc> {
        event.envelope.occurred_at
    }

    /// The run this event belongs to (`correlation_id` is always the run id).
    pub(crate) fn run_id(event: &BusEvent) -> Uuid {
        event.envelope.correlation_id
    }

    /// Usage split into the scalar columns every read model carries.
    pub(crate) struct UsageCols {
        pub input: i64,
        pub output: i64,
        pub cache_read: i64,
        pub cache_write: i64,
        pub wall_ms: i64,
        pub cost: Option<String>,
    }

    impl UsageCols {
        pub(crate) fn new(usage: &Usage) -> Self {
            Self {
                input: clamp(usage.input_tokens),
                output: clamp(usage.output_tokens),
                cache_read: clamp(usage.cache_read_tokens),
                cache_write: clamp(usage.cache_write_tokens),
                wall_ms: clamp(usage.wall_ms),
                cost: usage.cost_usd.map(|c| c.to_string()),
            }
        }
    }

    pub(crate) fn clamp(value: u64) -> i64 {
        i64::try_from(value).unwrap_or(i64::MAX)
    }

    /// Excerpt of `text` for list views (first line, ≤ `max` chars).
    pub(crate) fn excerpt(text: &str, max: usize) -> String {
        let line = text.lines().next().unwrap_or("").trim();
        if line.chars().count() <= max {
            return line.to_owned();
        }
        let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }

    /// Attempt number of `attempt_id`, read back from `core.events`
    /// (`task.attempt_started` carries both). Deterministic and always
    /// committed before any later attempt event, so it works identically
    /// during a live apply and during a rebuild.
    pub(crate) async fn attempt_no(
        conn: &mut PgConnection,
        task_id: Uuid,
        attempt_id: Uuid,
    ) -> Result<Option<i32>> {
        let no: Option<i32> = sqlx::query_scalar(
            "SELECT (payload->>'attempt_no')::int FROM core.events \
             WHERE aggregate_type = 'task' AND aggregate_id = $1 \
               AND event_type = 'task.attempt_started' AND payload->>'attempt_id' = $2 \
             ORDER BY position LIMIT 1",
        )
        .bind(task_id)
        .bind(attempt_id.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        Ok(no)
    }

    /// Kind of `task_id`, read back from its `task.created` event.
    pub(crate) async fn task_kind(
        conn: &mut PgConnection,
        task_id: Uuid,
    ) -> Result<Option<String>> {
        let kind: Option<String> = sqlx::query_scalar(
            "SELECT payload->>'kind' FROM core.events \
             WHERE aggregate_type = 'task' AND aggregate_id = $1 AND event_type = 'task.created' \
             ORDER BY position LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
        Ok(kind)
    }
}
