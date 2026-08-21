//! [`AppError`] — the failure type of every application service
//! (`plan/05-orchestration.md` §1).

use kevin_domain::DomainError;
use kevin_store::StoreError;
use serde_json::Value;

/// Why an application service could not apply a command.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AppError {
    /// The aggregate does not exist.
    #[error("{aggregate} {id} not found")]
    NotFound {
        /// Aggregate type (`run`, `task`, `question`).
        aggregate: &'static str,
        /// The id that was looked up.
        id: uuid::Uuid,
    },
    /// The aggregate rejected the command.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// Optimistic concurrency lost after the bounded retry budget.
    #[error("optimistic concurrency conflict on {stream} after {attempts} attempts")]
    Conflict {
        /// The stream that kept moving under us.
        stream: String,
        /// How many times the command was replayed.
        attempts: u32,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The command id was already processed; this is the recorded result.
    ///
    /// Services return the recorded result directly where the result type can
    /// be decoded, so callers normally never see this variant; it surfaces
    /// when the stored result cannot be decoded into the expected type.
    #[error("command already processed")]
    Duplicate(Value),
    /// A stored payload could not be decoded into the aggregate's event type.
    #[error("corrupt stream {stream}: {message}")]
    Corrupt {
        /// The stream.
        stream: String,
        /// What was wrong.
        message: String,
    },
    /// A port (router, roles, memory, evaluator, workspace) failed.
    #[error("{0}")]
    Port(#[from] crate::ports::PortError),
}

impl AppError {
    /// `true` when the caller may safely retry the command as-is.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        matches!(self, AppError::Conflict { .. }) || matches!(self, AppError::Store(_))
    }

    /// `true` for [`AppError::Domain`] with an invalid-transition cause; the
    /// saga treats those as "already done" and moves on.
    #[must_use]
    pub const fn is_invalid_transition(&self) -> bool {
        matches!(
            self,
            AppError::Domain(DomainError::InvalidTransition { .. })
        )
    }
}
