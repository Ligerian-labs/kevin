//! Aggregate snapshots (`core.snapshots`): one JSON state per stream, keyed by
//! the stream and stamped with the `aggregate_version` it reflects. Rehydrate
//! with `load(stream)` then `load_stream(stream, snapshot.version)`.

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::event_store::{StreamId, to_i64, to_u64};

/// A stored snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Stream version the state reflects.
    pub version: u64,
    /// Serialised aggregate state.
    pub state: Value,
    /// When it was saved.
    pub taken_at: DateTime<Utc>,
}

/// Access to `core.snapshots`.
#[derive(Debug, Clone)]
pub struct Snapshots {
    pool: PgPool,
}

impl Snapshots {
    /// Wraps a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upserts the snapshot of `stream` at `version` (never moves backwards: a
    /// save with a lower version than the stored one is ignored).
    pub async fn save(
        &self,
        stream: &StreamId,
        version: u64,
        state: &Value,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO core.snapshots (aggregate_type, aggregate_id, aggregate_version, state, taken_at) \
             VALUES ($1, $2, $3, $4, now()) \
             ON CONFLICT (aggregate_type, aggregate_id) DO UPDATE \
             SET aggregate_version = EXCLUDED.aggregate_version, state = EXCLUDED.state, taken_at = now() \
             WHERE core.snapshots.aggregate_version < EXCLUDED.aggregate_version",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .bind(to_i64(version)?)
        .bind(state)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The latest snapshot of `stream`, if any.
    pub async fn load(&self, stream: &StreamId) -> Result<Option<Snapshot>, StoreError> {
        let row: Option<(i64, Value, DateTime<Utc>)> = sqlx::query_as(
            "SELECT aggregate_version, state, taken_at FROM core.snapshots \
             WHERE aggregate_type = $1 AND aggregate_id = $2",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(version, state, taken_at)| {
            Ok(Snapshot {
                version: to_u64(version, "core.snapshots", "aggregate_version")?,
                state,
                taken_at,
            })
        })
        .transpose()
    }

    /// Deletes the snapshot of `stream`; returns whether one existed.
    pub async fn delete(&self, stream: &StreamId) -> Result<bool, StoreError> {
        let done = sqlx::query(
            "DELETE FROM core.snapshots WHERE aggregate_type = $1 AND aggregate_id = $2",
        )
        .bind(stream.aggregate_type)
        .bind(stream.aggregate_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }
}
