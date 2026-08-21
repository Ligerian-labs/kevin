//! Projection checkpoints (`core.projection_checkpoints`): the last global
//! position each named consumer has processed. Consumers resume with
//! `read_all(checkpoint, limit)`.

use sqlx::PgPool;

use crate::error::StoreError;
use crate::event_store::{to_i64, to_u64};

/// Access to `core.projection_checkpoints`.
#[derive(Debug, Clone)]
pub struct Checkpoints {
    pool: PgPool,
}

impl Checkpoints {
    /// Wraps a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Last processed position of `name` (`None` when it never checkpointed).
    pub async fn get(&self, name: &str) -> Result<Option<u64>, StoreError> {
        let position: Option<i64> =
            sqlx::query_scalar("SELECT position FROM core.projection_checkpoints WHERE name = $1")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        position
            .map(|p| to_u64(p, "core.projection_checkpoints", "position"))
            .transpose()
    }

    /// Records that `name` processed everything up to and including `position`.
    pub async fn set(&self, name: &str, position: u64) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO core.projection_checkpoints (name, position, updated_at) \
             VALUES ($1, $2, now()) \
             ON CONFLICT (name) DO UPDATE SET position = EXCLUDED.position, updated_at = now()",
        )
        .bind(name)
        .bind(to_i64(position)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forgets `name`'s checkpoint (used by `rebuild-projection`). Returns
    /// whether a row existed.
    pub async fn delete(&self, name: &str) -> Result<bool, StoreError> {
        let done = sqlx::query("DELETE FROM core.projection_checkpoints WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Every checkpoint, ordered by name.
    pub async fn list(&self) -> Result<Vec<(String, u64)>, StoreError> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, position FROM core.projection_checkpoints ORDER BY name")
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(name, p)| Ok((name, to_u64(p, "core.projection_checkpoints", "position")?)))
            .collect()
    }
}
