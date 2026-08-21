//! [`DomainError`]: every reason an aggregate can reject a command.
//!
//! Errors are data (`Clone + PartialEq`) so tests can assert on them exactly
//! and services can map them to API error codes without string matching.

use rust_decimal::Decimal;
use uuid::Uuid;

use crate::ids::{AttemptId, ProposalId, QuestionId, TaskId};
use crate::plan::PlanError;
use crate::values::{BudgetDimension, BudgetExcess, InvalidValue, ProposalStatus};

/// Why an aggregate rejected a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// The command is not allowed in the aggregate's current state.
    #[error("{aggregate} in state `{from}` cannot handle `{command}`")]
    InvalidTransition {
        /// Aggregate type (`run`, `task`, …).
        aggregate: &'static str,
        /// Current state name (`snake_case`).
        from: &'static str,
        /// Command name (`snake_case`).
        command: &'static str,
    },
    /// The aggregate does not exist yet (no creating event applied).
    #[error("{aggregate} {id} does not exist")]
    NotFound {
        /// Aggregate type.
        aggregate: &'static str,
        /// Requested id.
        id: Uuid,
    },
    /// The creating command was sent to an aggregate that already exists.
    #[error("{aggregate} {id} already exists")]
    AlreadyExists {
        /// Aggregate type.
        aggregate: &'static str,
        /// Existing id.
        id: Uuid,
    },
    /// A budget limit was crossed.
    #[error("budget exhausted: {dimension} limit {limit} exceeded by {actual}")]
    BudgetExhausted {
        /// Which limit.
        dimension: BudgetDimension,
        /// The limit.
        limit: Decimal,
        /// The observed value.
        actual: Decimal,
    },
    /// No more attempts are allowed on the task.
    #[error("attempts exhausted: {attempts} of {max}")]
    AttemptsExhausted {
        /// Attempts so far.
        attempts: u8,
        /// `Budget::max_attempts`.
        max: u8,
    },
    /// The last failure class does not allow a retry.
    #[error("last attempt failed with a non-retryable class ({class})")]
    NotRetryable {
        /// The failure class.
        class: crate::kinds::FailureClass,
    },
    /// The question already has an answer.
    #[error("question already answered")]
    AlreadyAnswered,
    /// A blocking question cannot expire.
    #[error("question with policy `block` never expires")]
    QuestionDoesNotExpire,
    /// A task already has an attempt in flight.
    #[error("attempt {attempt_id} is already running")]
    AttemptAlreadyRunning {
        /// The running attempt.
        attempt_id: AttemptId,
    },
    /// The command names an attempt that is not the active one.
    #[error("attempt {got} is not the active attempt ({expected:?})")]
    AttemptMismatch {
        /// The active attempt, if any.
        expected: Option<AttemptId>,
        /// The attempt named by the command.
        got: AttemptId,
    },
    /// `StartAttempt` needs a route first.
    #[error("task has no route")]
    RouteRequired,
    /// The command references a question the aggregate does not know.
    #[error("question {question_id} is not pending on this aggregate")]
    UnknownQuestion {
        /// The question.
        question_id: QuestionId,
    },
    /// The command references a task the run does not know.
    #[error("task {task_id} does not belong to this run")]
    UnknownTask {
        /// The task.
        task_id: TaskId,
    },
    /// The run has no plan (or it was not approved) where one is required.
    #[error("run has no approved plan")]
    PlanRequired,
    /// The proposed plan failed validation.
    #[error("invalid plan: {}", format_plan_errors(.0))]
    InvalidPlan(Vec<PlanError>),
    /// The answer does not fit the question.
    #[error("invalid answer: {reason}")]
    InvalidAnswer {
        /// Why.
        reason: String,
    },
    /// No proposal with that id on the evaluation.
    #[error("unknown proposal {proposal_id}")]
    UnknownProposal {
        /// The proposal.
        proposal_id: ProposalId,
    },
    /// The proposal was already accepted or rejected.
    #[error("proposal {proposal_id} already decided ({status:?})")]
    ProposalAlreadyDecided {
        /// The proposal.
        proposal_id: ProposalId,
        /// Its current status.
        status: ProposalStatus,
    },
    /// A value in the command failed validation.
    #[error(transparent)]
    InvalidValue(#[from] InvalidValue),
}

impl DomainError {
    /// Shorthand for [`DomainError::InvalidValue`].
    pub fn invalid_value(field: impl Into<String>, reason: impl Into<String>) -> Self {
        DomainError::InvalidValue(InvalidValue::new(field, reason))
    }

    /// Shorthand for [`DomainError::InvalidTransition`].
    #[must_use]
    pub const fn invalid_transition(
        aggregate: &'static str,
        from: &'static str,
        command: &'static str,
    ) -> Self {
        DomainError::InvalidTransition {
            aggregate,
            from,
            command,
        }
    }
}

impl From<BudgetExcess> for DomainError {
    fn from(excess: BudgetExcess) -> Self {
        DomainError::BudgetExhausted {
            dimension: excess.dimension,
            limit: excess.limit,
            actual: excess.actual,
        }
    }
}

fn format_plan_errors(errors: &[PlanError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
