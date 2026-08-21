//! Errors of the evaluation context.

use std::time::Duration;

use kevin_domain::{DomainError, EvaluationId, FailureClass, ModelAlias, ProposalId, WorkerKind};

use crate::judge::JudgeOutputError;
use crate::rubric::RubricError;

/// `Result` of this crate.
pub type Result<T> = std::result::Result<T, EvaluatorError>;

/// Why an evaluation could not be produced or applied.
#[derive(Debug, thiserror::Error)]
pub enum EvaluatorError {
    /// Configuration switched this evaluation off; the caller does nothing.
    #[error("evaluation skipped: {0}")]
    Skipped(#[from] SkipReason),
    /// The rubric could not be loaded.
    #[error(transparent)]
    Rubric(#[from] RubricError),
    /// The judge's answer is unusable (after the one repair turn).
    #[error(transparent)]
    JudgeOutput(#[from] JudgeOutputError),
    /// No worker of that kind is registered.
    #[error("judge worker `{worker}` is not available")]
    WorkerUnavailable {
        /// The requested worker.
        worker: WorkerKind,
    },
    /// The judge route names an alias that is not configured.
    #[error("judge model alias `{alias}` is not configured")]
    UnknownModel {
        /// The requested alias.
        alias: ModelAlias,
    },
    /// The judge call did not finish in time.
    #[error("judge call timed out after {0:?}")]
    Timeout(Duration),
    /// The judge worker failed.
    #[error("judge call failed ({}): {message}", class.as_str())]
    JudgeFailed {
        /// Failure class as classified by the adapter.
        class: FailureClass,
        /// Diagnostic.
        message: String,
    },
    /// The worker could not be spawned.
    #[error("cannot start the judge worker: {0}")]
    Spawn(#[source] kevin_worker::WorkerError),
    /// The `Evaluation` aggregate rejected a command.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// The evaluation or proposal is unknown.
    #[error("evaluation `{0}` not found")]
    EvaluationNotFound(EvaluationId),
    /// The proposal is unknown.
    #[error("proposal `{0}` not found")]
    ProposalNotFound(ProposalId),
    /// Persistence failed.
    #[error("evaluation store: {0}")]
    Store(String),
    /// Applying an auto-apply part failed.
    #[error("auto-apply ({part}): {message}")]
    AutoApply {
        /// `routing` or `memory`.
        part: &'static str,
        /// What went wrong.
        message: String,
    },
}

impl EvaluatorError {
    /// A [`EvaluatorError::Store`] from any displayable error.
    pub fn store(err: impl std::fmt::Display) -> Self {
        EvaluatorError::Store(err.to_string())
    }

    /// `true` when the evaluation was skipped by configuration rather than
    /// failing (the caller treats it as "nothing to do").
    #[must_use]
    pub const fn is_skipped(&self) -> bool {
        matches!(self, EvaluatorError::Skipped(_))
    }
}

/// Why an evaluation did not run at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkipReason {
    /// `evaluation.enabled = false`.
    #[error("`evaluation.enabled = false`")]
    Disabled,
    /// `evaluation.evaluate_tasks = false` and the subject is a task.
    #[error("`evaluation.evaluate_tasks = false`")]
    TasksDisabled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_is_not_a_failure() {
        let err = EvaluatorError::from(SkipReason::TasksDisabled);
        assert!(err.is_skipped());
        assert!(err.to_string().contains("evaluate_tasks"));
        assert!(!EvaluatorError::Store("boom".into()).is_skipped());
    }
}
