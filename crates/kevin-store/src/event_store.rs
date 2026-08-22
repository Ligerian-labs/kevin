//! The event store (`plan/01-architecture.md` §Event-driven core, `plan/02` §Event envelope).
//!
//! # Contract (frozen, `plan/12-workstreams.md` WS-03)
//!
//! - [`EventStore::append`] — appends to one aggregate stream with optimistic
//!   concurrency (`expected_version` = version the caller loaded; `0` for a new
//!   stream). Events and their `core.outbox` rows are written in **one
//!   transaction**; `pg_notify('kevin_events', <last position>)` is emitted
//!   with the commit (`NOTIFY` is transactional — listeners only see it once the
//!   events are visible).
//! - [`EventStore::load_stream`] — events of one stream with
//!   `aggregate_version > from_version` (`0` = whole stream; pass a snapshot's
//!   version to read what came after it).
//! - [`EventStore::read_all`] — events with `position > from_position`, at
//!   most `limit`, in position order (`0` = from the beginning; pass a
//!   checkpoint to resume).
//! - [`EventStore::subscribe_positions`] — a `watch` of the highest position
//!   this store instance knows to be committed (bumped after every local
//!   append; the bus may feed positions learnt from `NOTIFY` through
//!   [`PgEventStore::note_position`]).
//!
//! # Global position semantics
//!
//! `core.events.position` is a `BIGSERIAL`. Every append takes a
//! transaction-scoped advisory lock ([`APPEND_LOCK_KEY`]) **before** allocating
//! positions, so appends are serialised: positions are allocated in commit
//! order and become visible in order. A reader that has seen position `P` is
//! guaranteed that every position `< P` is already committed — catch-up by
//! "read everything `> checkpoint`" never skips an event. Gaps are possible
//! only when a transaction that already allocated positions fails after the
//! conflict check (e.g. connection loss); such gaps are permanent and readers
//! must never wait for them to fill. `ac_ws03_2_global_ordering_and_catch_up`
//! asserts the ordering and the no-skip property under concurrent writers.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kevin_bus::{BusEvent, EventSource, SourceError};
use kevin_domain::{Actor, EventEnvelope, EventId};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{FromRow, PgPool};
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::StoreError;
use crate::upcast::Upcasters;

/// Postgres `NOTIFY` channel on which every append publishes its last position.
pub const NOTIFY_CHANNEL: &str = "kevin_events";

/// Key of the transaction-scoped advisory lock serialising appends
/// (`pg_advisory_xact_lock`). Arbitrary but fixed; documented so other tools
/// never reuse it.
pub const APPEND_LOCK_KEY: i64 = 0x4b45_5649_4e5f_4556; // "KEVIN_EV"

/// Identifies one aggregate stream: `(aggregate_type, aggregate_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId {
    /// `"run"`, `"task"`, … (an aggregate's `TYPE` constant).
    pub aggregate_type: &'static str,
    /// The aggregate id.
    pub aggregate_id: Uuid,
}

impl StreamId {
    /// Builds a stream id.
    pub fn new(aggregate_type: &'static str, aggregate_id: impl Into<Uuid>) -> Self {
        Self {
            aggregate_type,
            aggregate_id: aggregate_id.into(),
        }
    }
}

impl fmt::Display for StreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.aggregate_type, self.aggregate_id)
    }
}

/// An event to append. The store assigns `aggregate_version` and `position`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    /// uuid v7 of the event (from the injected `IdGen`).
    pub event_id: EventId,
    /// `"<context>.<past_tense>"`, e.g. `"run.started"`.
    pub event_type: &'static str,
    /// Schema version of `payload` for this event type.
    pub schema_version: u16,
    /// When it occurred (from the injected `Clock`).
    pub occurred_at: DateTime<Utc>,
    /// The `RunId` when one exists.
    pub correlation_id: Uuid,
    /// The command or event that caused it.
    pub causation_id: Option<Uuid>,
    /// Who caused it.
    pub actor: Actor,
    /// The event payload as JSON.
    pub payload: Value,
}

/// An event read back from the store: its global position plus the envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEvent {
    /// Global position (`core.events.position`, strictly increasing).
    pub position: u64,
    /// The envelope (payload already upcast to the latest known schema version).
    pub envelope: EventEnvelope<Value>,
}

impl std::ops::Deref for StoredEvent {
    type Target = EventEnvelope<Value>;

    fn deref(&self) -> &Self::Target {
        &self.envelope
    }
}

impl StoredEvent {
    /// The stream this event belongs to.
    #[must_use]
    pub fn stream(&self) -> StreamId {
        StreamId::new(self.envelope.aggregate_type, self.envelope.aggregate_id)
    }
}

/// Outcome of a successful append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendResult {
    /// The stream appended to.
    pub stream: StreamId,
    /// Stream version after the append (`expected_version + events.len()`).
    pub new_version: u64,
    /// Global position of the first appended event.
    pub first_position: u64,
    /// Global position of the last appended event.
    pub last_position: u64,
    /// The appended events as stored (what the caller publishes on the bus after commit).
    pub events: Vec<StoredEvent>,
}

/// The event store contract other crates code against.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Appends `events` to `stream`, which must currently be at `expected_version`
    /// (`0` for a new stream). Returns [`StoreError::VersionConflict`] otherwise.
    async fn append(
        &self,
        stream: &StreamId,
        expected_version: u64,
        events: &[NewEvent],
    ) -> Result<AppendResult, StoreError>;

    /// Events of `stream` with `aggregate_version > from_version`, in version order.
    async fn load_stream(
        &self,
        stream: &StreamId,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// Events with `position > from_position`, at most `limit`, in position order.
    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError>;

    /// Highest global position this store instance knows to be committed.
    fn subscribe_positions(&self) -> watch::Receiver<u64>;
}

/// Postgres implementation of [`EventStore`] over `core.events` + `core.outbox`.
#[derive(Debug, Clone)]
pub struct PgEventStore {
    pool: PgPool,
    positions: Arc<watch::Sender<u64>>,
    upcasters: Arc<Upcasters>,
}

impl PgEventStore {
    /// Creates a store with the domain upcaster registry
    /// ([`Upcasters::domain`]), so every read returns the latest payload shape.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self::with_upcasters(pool, Upcasters::domain())
    }

    /// Creates a store that applies `upcasters` to every event it reads.
    #[must_use]
    pub fn with_upcasters(pool: PgPool, upcasters: Upcasters) -> Self {
        let (tx, _rx) = watch::channel(0);
        Self {
            pool,
            positions: Arc::new(tx),
            upcasters: Arc::new(upcasters),
        }
    }

    /// The underlying pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The upcaster registry applied on read.
    #[must_use]
    pub fn upcasters(&self) -> &Upcasters {
        &self.upcasters
    }

    /// Records that `position` is committed (monotone: lower values are
    /// ignored). The bus calls this when a `NOTIFY` from another process
    /// arrives so local subscribers wake up.
    pub fn note_position(&self, position: u64) {
        self.positions.send_if_modified(|current| {
            if position > *current {
                *current = position;
                true
            } else {
                false
            }
        });
    }

    /// Highest position currently in the table (`0` when empty). Also seeds the
    /// position watch, so call it once after construction if you want
    /// `subscribe_positions()` to start from the database's head rather than `0`.
    pub async fn head_position(&self) -> Result<u64, StoreError> {
        let head: Option<i64> = sqlx::query_scalar("SELECT max(position) FROM core.events")
            .fetch_one(&self.pool)
            .await?;
        let head = to_u64(head.unwrap_or(0), "core.events", "position")?;
        self.note_position(head);
        Ok(head)
    }

    /// Current version of `stream` (`0` when it does not exist).
    pub async fn stream_version(&self, stream: &StreamId) -> Result<u64, StoreError> {
        let version: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(aggregate_version), 0) FROM core.events \
             WHERE aggregate_type = $1 AND aggregate_id = $2",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .fetch_one(&self.pool)
        .await?;
        to_u64(version, "core.events", "aggregate_version")
    }

    fn finish(&self, row: EventRow) -> Result<StoredEvent, StoreError> {
        let stored = row.into_stored()?;
        Ok(StoredEvent {
            position: stored.position,
            envelope: self.upcasters.apply(stored.envelope),
        })
    }
}

#[async_trait]
impl EventStore for PgEventStore {
    async fn append(
        &self,
        stream: &StreamId,
        expected_version: u64,
        events: &[NewEvent],
    ) -> Result<AppendResult, StoreError> {
        if events.is_empty() {
            return Err(StoreError::EmptyAppend { stream: *stream });
        }
        let expected = to_i64(expected_version)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(APPEND_LOCK_KEY)
            .execute(&mut *tx)
            .await?;
        let actual: i64 = sqlx::query_scalar(
            "SELECT coalesce(max(aggregate_version), 0) FROM core.events \
             WHERE aggregate_type = $1 AND aggregate_id = $2",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .fetch_one(&mut *tx)
        .await?;
        if actual != expected {
            tx.rollback().await?;
            return Err(StoreError::VersionConflict {
                stream: *stream,
                expected: expected_version,
                actual: to_u64(actual, "core.events", "aggregate_version")?,
            });
        }

        let mut stored = Vec::with_capacity(events.len());
        let mut version = expected;
        let redactor = kevin_telemetry::redact::Redactor::global();
        for event in events {
            version += 1;
            let actor = serde_json::to_value(&event.actor)?;
            // `plan/09-security.md` §Redaction: the event store is a *sink*.
            // Nothing that reaches `core.events` may carry a credential — the
            // rows are kept forever and every projection, SSE stream and
            // transcript view is derived from them, so redacting here covers
            // all of them at once (T3).
            let mut payload = event.payload.clone();
            redactor.redact_value(&mut payload);
            let row = sqlx::query_as::<_, EventRow>(
                "INSERT INTO core.events (event_id, event_type, schema_version, occurred_at, \
                 aggregate_type, aggregate_id, aggregate_version, correlation_id, causation_id, \
                 actor, payload) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
                 RETURNING position, event_id, event_type, schema_version, occurred_at, \
                 aggregate_type, aggregate_id, aggregate_version, correlation_id, causation_id, \
                 actor, payload",
            )
            .bind(event.event_id.as_uuid())
            .bind(event.event_type)
            .bind(i32::from(event.schema_version))
            .bind(event.occurred_at)
            .bind(stream.aggregate_type)
            .bind(stream.aggregate_id)
            .bind(version)
            .bind(event.correlation_id)
            .bind(event.causation_id)
            .bind(actor)
            .bind(&payload)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_unique_violation(e, stream, expected_version, actual))?;
            let row = row.into_stored()?;
            sqlx::query("INSERT INTO core.outbox (position, event_id) VALUES ($1, $2)")
                .bind(to_i64(row.position)?)
                .bind(event.event_id.as_uuid())
                .execute(&mut *tx)
                .await?;
            stored.push(row);
        }
        let first_position = stored.first().map_or(0, |e| e.position);
        let last_position = stored.last().map_or(0, |e| e.position);
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(NOTIFY_CHANNEL)
            .bind(last_position.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.note_position(last_position);

        Ok(AppendResult {
            stream: *stream,
            new_version: to_u64(version, "core.events", "aggregate_version")?,
            first_position,
            last_position,
            events: stored,
        })
    }

    async fn load_stream(
        &self,
        stream: &StreamId,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let rows = sqlx::query_as::<_, EventRow>(
            "SELECT position, event_id, event_type, schema_version, occurred_at, aggregate_type, \
             aggregate_id, aggregate_version, correlation_id, causation_id, actor, payload \
             FROM core.events \
             WHERE aggregate_type = $1 AND aggregate_id = $2 AND aggregate_version > $3 \
             ORDER BY aggregate_version",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .bind(to_i64(from_version)?)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| self.finish(r)).collect()
    }

    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query_as::<_, EventRow>(
            "SELECT position, event_id, event_type, schema_version, occurred_at, aggregate_type, \
             aggregate_id, aggregate_version, correlation_id, causation_id, actor, payload \
             FROM core.events WHERE position > $1 ORDER BY position LIMIT $2",
        )
        .bind(to_i64(from_position)?)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| self.finish(r)).collect()
    }

    fn subscribe_positions(&self) -> watch::Receiver<u64> {
        self.positions.subscribe()
    }
}

/// Raw `core.events` row. `pub(crate)` so the outbox relay can reuse it.
#[derive(Debug, FromRow)]
pub(crate) struct EventRow {
    position: i64,
    event_id: Uuid,
    event_type: String,
    schema_version: i32,
    occurred_at: DateTime<Utc>,
    aggregate_type: String,
    aggregate_id: Uuid,
    aggregate_version: i64,
    correlation_id: Uuid,
    causation_id: Option<Uuid>,
    actor: Value,
    payload: Value,
}

impl EventRow {
    /// Maps the row to a [`StoredEvent`] **without** upcasting.
    pub(crate) fn into_stored(self) -> Result<StoredEvent, StoreError> {
        let schema_version =
            u16::try_from(self.schema_version).map_err(|_| StoreError::Corrupt {
                table: "core.events",
                message: format!("schema_version {} out of range", self.schema_version),
            })?;
        let actor: Actor = serde_json::from_value(self.actor)?;
        Ok(StoredEvent {
            position: to_u64(self.position, "core.events", "position")?,
            envelope: EventEnvelope {
                event_id: EventId::from_uuid(self.event_id),
                event_type: kevin_domain::envelope::intern(&self.event_type),
                schema_version,
                occurred_at: self.occurred_at,
                aggregate_type: kevin_domain::envelope::intern(&self.aggregate_type),
                aggregate_id: self.aggregate_id,
                aggregate_version: to_u64(
                    self.aggregate_version,
                    "core.events",
                    "aggregate_version",
                )?,
                correlation_id: self.correlation_id,
                causation_id: self.causation_id,
                actor,
                payload: self.payload,
            },
        })
    }
}

impl TryFrom<PgRow> for StoredEvent {
    type Error = StoreError;

    fn try_from(row: PgRow) -> Result<Self, Self::Error> {
        EventRow::from_row(&row)?.into_stored()
    }
}

/// Maps a unique-constraint violation on the stream index (only reachable if
/// something appends without the advisory lock) to a `VersionConflict`.
fn map_unique_violation(
    err: sqlx::Error,
    stream: &StreamId,
    expected: u64,
    actual: i64,
) -> StoreError {
    let is_stream_unique = err.as_database_error().is_some_and(|db| {
        db.is_unique_violation() && db.constraint() == Some("events_stream_version_unique")
    });
    if is_stream_unique {
        StoreError::VersionConflict {
            stream: *stream,
            expected,
            actual: u64::try_from(actual).unwrap_or(expected),
        }
    } else {
        StoreError::Database(err)
    }
}

pub(crate) fn to_u64(value: i64, table: &'static str, column: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt {
        table,
        message: format!("{column} = {value} is negative"),
    })
}

pub(crate) fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::InvalidConfig(format!("{value} exceeds i64")))
}

/// Convenience: reads `position` from a `NOTIFY` payload (`None` if malformed).
#[must_use]
pub fn parse_notify_payload(payload: &str) -> Option<u64> {
    payload.trim().parse().ok()
}

/// The store is the bus' source of truth: [`PgNotifyBus`] reads every event
/// back by global position from here, so `NOTIFY` stays a wake-up hint
/// (`plan/01-architecture.md` §Event-driven core; `kevin_bus` module docs
/// name `kevin-store` as the [`EventSource`] implementor).
///
/// [`PgNotifyBus`]: kevin_bus::PgNotifyBus
#[async_trait]
impl EventSource for PgEventStore {
    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<BusEvent>, SourceError> {
        let events = EventStore::read_all(self, from_position, limit)
            .await
            .map_err(SourceError::new)?;
        Ok(events
            .into_iter()
            .map(|e| BusEvent::new(e.position, e.envelope))
            .collect())
    }

    async fn latest_position(&self) -> Result<u64, SourceError> {
        self.head_position().await.map_err(SourceError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_id_displays_type_and_id() {
        let id = Uuid::nil();
        assert_eq!(
            StreamId::new("run", id).to_string(),
            "run/00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn notify_payload_parses_positions() {
        assert_eq!(parse_notify_payload("42"), Some(42));
        assert_eq!(parse_notify_payload(" 7\n"), Some(7));
        assert_eq!(parse_notify_payload("nope"), None);
    }

    #[tokio::test]
    async fn watch_is_monotone() {
        let (tx, rx) = watch::channel(0u64);
        let store = PgEventStore {
            pool: PgPool::connect_lazy("postgres://localhost/unused").expect("lazy pool"),
            positions: Arc::new(tx),
            upcasters: Arc::new(Upcasters::new()),
        };
        store.note_position(5);
        store.note_position(3);
        assert_eq!(*rx.borrow(), 5);
    }
}
