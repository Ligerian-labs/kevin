//! Routing errors (`plan/06-memory-and-learning.md` §2.2).

use kevin_domain::{DomainError, ModelAlias, TaskKind};

/// Why a routing operation failed.
#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    /// No candidate survived the filters (task fails `Permanent`).
    #[error(
        "no route available for task kind `{task_kind}`: {reason} \
         (configure `[routing.kinds.{task_kind}].candidates` or `[roles].default`)"
    )]
    NoRoute {
        /// The kind that could not be routed.
        task_kind: TaskKind,
        /// Why every candidate was rejected.
        reason: String,
    },
    /// A configured candidate or role alias is not in `[models]`.
    #[error("unknown model alias `{alias}` (referenced by {referenced_by})")]
    UnknownAlias {
        /// The missing alias.
        alias: ModelAlias,
        /// Config key that referenced it.
        referenced_by: String,
    },
    /// A `RouteScore` command was rejected by the domain.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// The score store failed.
    #[error(transparent)]
    Store(#[from] kevin_store::StoreError),
}

impl From<sqlx::Error> for RoutingError {
    fn from(err: sqlx::Error) -> Self {
        RoutingError::Store(kevin_store::StoreError::from(err))
    }
}
