//! Error type of the store.

use crate::event_store::StreamId;

/// Every failure the store can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// Optimistic concurrency check failed: the stream is not at `expected`.
    #[error(
        "version conflict on stream {stream}: expected version {expected}, stream is at {actual}"
    )]
    VersionConflict {
        /// The stream that was appended to.
        stream: StreamId,
        /// Version the caller expected the stream to be at.
        expected: u64,
        /// Version the stream is actually at.
        actual: u64,
    },

    /// `append` was called with zero events (a caller bug; never a no-op).
    #[error("append to stream {stream} with no events")]
    EmptyAppend {
        /// The stream the caller tried to append to.
        stream: StreamId,
    },

    /// The migrations policy was `CheckOnly` and migrations are pending.
    #[error("migrations pending: {pending:?} (run `kevin db migrate`)")]
    MigrationsPending {
        /// Versions that are not applied yet.
        pending: Vec<i64>,
    },

    /// Applied migrations differ from the embedded ones (checksum mismatch or
    /// a version applied that this binary does not know).
    #[error("migration {version} was applied with different contents or is unknown to this binary")]
    MigrationMismatch {
        /// The offending migration version.
        version: i64,
    },

    /// The `vector` extension is not installed in the target database.
    #[error(
        "pgvector extension `vector` is not installed (run `kevin db init` or `CREATE EXTENSION vector` as a superuser)"
    )]
    PgVectorMissing,

    /// Invalid database configuration (bad URL, zero pool size, …).
    #[error("invalid database configuration: {0}")]
    InvalidConfig(String),

    /// A stored value could not be mapped to the expected Rust type.
    #[error("corrupt row in {table}: {message}")]
    Corrupt {
        /// Table the row was read from.
        table: &'static str,
        /// What was wrong.
        message: String,
    },

    /// JSON (de)serialisation failure.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    /// Migration runner failure.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),

    /// Any other database error.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl StoreError {
    /// Whether this is an optimistic-concurrency conflict.
    #[must_use]
    pub const fn is_version_conflict(&self) -> bool {
        matches!(self, StoreError::VersionConflict { .. })
    }

    /// Whether the database could not be reached (connection refused, pool
    /// acquire timeout, TLS handshake, …) as opposed to a query error.
    #[must_use]
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            StoreError::Database(
                sqlx::Error::Io(_)
                    | sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::Tls(_)
            )
        )
    }
}
