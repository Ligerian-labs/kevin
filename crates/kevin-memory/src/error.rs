//! Errors of the memory context.

use kevin_domain::MemoryItemId;

use crate::embed::EmbedError;
use crate::item::MAX_CONTENT_CHARS;

/// Result alias of this crate.
pub type Result<T> = std::result::Result<T, MemoryError>;

/// Everything memory can refuse or fail at.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// `memory.enabled = false`: nothing is stored or retrieved.
    #[error("memory is disabled (`memory.enabled = false`)")]
    Disabled,

    /// The content matched a secret pattern and was **not** stored
    /// (`plan/09-security.md` §Memory privacy).
    #[error(
        "refusing to store memory content: it contains {kinds} (redacted {percent}% of the text)"
    )]
    ContainsSecret {
        /// Redaction markers found, e.g. `anthropic_key, bearer`.
        kinds: String,
        /// Share of the text that redaction replaced, in percent.
        percent: u8,
    },

    /// Content is empty or only whitespace.
    #[error("refusing to store an empty memory item")]
    EmptyContent,

    /// Content is longer than the column allows.
    #[error("memory content is {len} characters, the maximum is {MAX_CONTENT_CHARS}")]
    TooLong {
        /// Length of the offending content.
        len: usize,
    },

    /// The embedder's dimension does not match the `vector(N)` column (a
    /// model change needs a migration for the column *and* `kevin memory reindex`).
    #[error(
        "embedding dimension mismatch: model `{model}` produces {actual} dimensions but the \
         column/config expects {expected} (change the column with a migration, then run \
         `kevin memory reindex`)"
    )]
    DimensionMismatch {
        /// Model that produced the vectors.
        model: String,
        /// What the column (or `memory.dimensions`) expects.
        expected: usize,
        /// What the embedder produces.
        actual: usize,
    },

    /// No such item.
    #[error("memory item {0} not found")]
    NotFound(MemoryItemId),

    /// The item exists but was already forgotten.
    #[error("memory item {0} was already forgotten")]
    AlreadyForgotten(MemoryItemId),

    /// A bad argument (unknown kind, malformed date…).
    #[error("invalid memory query: {0}")]
    Invalid(String),

    /// The embedder failed.
    #[error(transparent)]
    Embed(#[from] EmbedError),

    /// The event store failed.
    #[error(transparent)]
    Store(#[from] kevin_store::StoreError),

    /// JSON (de)serialisation failed.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    /// Any other database error.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl MemoryError {
    /// Whether the database could not be reached (as opposed to a query
    /// error) — the CLI maps this to its "unreachable" exit code.
    #[must_use]
    pub fn is_unreachable(&self) -> bool {
        match self {
            MemoryError::Database(err) => matches!(
                err,
                sqlx::Error::Io(_)
                    | sqlx::Error::PoolTimedOut
                    | sqlx::Error::PoolClosed
                    | sqlx::Error::Tls(_)
            ),
            MemoryError::Store(err) => err.is_unreachable(),
            _ => false,
        }
    }

    /// Whether the error is a refusal to store content (never retried).
    #[must_use]
    pub const fn is_refusal(&self) -> bool {
        matches!(
            self,
            MemoryError::ContainsSecret { .. }
                | MemoryError::EmptyContent
                | MemoryError::TooLong { .. }
        )
    }
}
