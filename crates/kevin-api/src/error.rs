//! The stable error envelope and its code table (`plan/07-api-and-tui.md`
//! §Conventions).
//!
//! Every non-2xx response is
//! `{ "code": …, "message": …, "details"?: …, "request_id": … }`. Codes are
//! **stable and language-neutral** — clients switch on `code`, never on
//! `message` (which is human-readable, redacted and may change).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every error code the API can return, with its HTTP status.
///
/// The list is exactly the table in plan/07 §Conventions; adding a code is a
/// plan change, not an implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "server", derive(utoipa::ToSchema))]
pub enum ErrorCode {
    // -- 400 ---------------------------------------------------------------
    /// Malformed body, bad query parameter, unparsable path id.
    InvalidRequest,
    /// The goal is empty or larger than 64 KiB.
    InvalidGoal,
    /// The answer selects unknown options or is empty for a blocking question.
    InvalidAnswer,
    /// The pagination cursor could not be decoded.
    InvalidCursor,
    /// The request body exceeded the 1 MiB limit.
    PayloadTooLarge,

    // -- 401 ---------------------------------------------------------------
    /// Missing or invalid bearer token.
    Unauthenticated,

    // -- 403 ---------------------------------------------------------------
    /// A loopback-only endpoint was reached from a remote address.
    Forbidden,

    // -- 404 ---------------------------------------------------------------
    /// No such run.
    RunNotFound,
    /// No such task.
    TaskNotFound,
    /// No such question.
    QuestionNotFound,
    /// No such proposal.
    ProposalNotFound,
    /// No such artifact.
    ArtifactNotFound,

    // -- 409 ---------------------------------------------------------------
    /// The `Idempotency-Key` was replayed with a different body.
    IdempotencyConflict,
    /// The run is not in a state that accepts this command.
    RunNotInState,
    /// The task is not in a state that accepts this command.
    TaskNotInState,
    /// The question already has an answer.
    QuestionAlreadyAnswered,
    /// Optimistic concurrency check failed; retry with a fresh read.
    VersionConflict,

    // -- 422 ---------------------------------------------------------------
    /// The plan is not a valid DAG / violates the plan rules.
    PlanInvalid,
    /// The requested budget is not usable (negative, above the operator cap…).
    BudgetInvalid,
    /// The request named a model alias that is not in the catalogue.
    UnknownModelAlias,
    /// The route's worker is disabled in the configuration.
    WorkerDisabled,

    // -- 429 ---------------------------------------------------------------
    /// The per-token rate limit or the SSE connection cap was hit.
    RateLimited,

    // -- 503 ---------------------------------------------------------------
    /// The runtime is draining and refuses new work.
    Draining,
    /// The database did not answer.
    DbUnavailable,
    /// The orchestrator is not wired up (yet) on this deployment.
    RuntimeUnavailable,

    // -- 500 ---------------------------------------------------------------
    /// Anything unexpected; the details never leak internals.
    Internal,
}

impl ErrorCode {
    /// The `snake_case` wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::InvalidGoal => "invalid_goal",
            ErrorCode::InvalidAnswer => "invalid_answer",
            ErrorCode::InvalidCursor => "invalid_cursor",
            ErrorCode::PayloadTooLarge => "payload_too_large",
            ErrorCode::Unauthenticated => "unauthenticated",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::RunNotFound => "run_not_found",
            ErrorCode::TaskNotFound => "task_not_found",
            ErrorCode::QuestionNotFound => "question_not_found",
            ErrorCode::ProposalNotFound => "proposal_not_found",
            ErrorCode::ArtifactNotFound => "artifact_not_found",
            ErrorCode::IdempotencyConflict => "idempotency_conflict",
            ErrorCode::RunNotInState => "run_not_in_state",
            ErrorCode::TaskNotInState => "task_not_in_state",
            ErrorCode::QuestionAlreadyAnswered => "question_already_answered",
            ErrorCode::VersionConflict => "version_conflict",
            ErrorCode::PlanInvalid => "plan_invalid",
            ErrorCode::BudgetInvalid => "budget_invalid",
            ErrorCode::UnknownModelAlias => "unknown_model_alias",
            ErrorCode::WorkerDisabled => "worker_disabled",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Draining => "draining",
            ErrorCode::DbUnavailable => "db_unavailable",
            ErrorCode::RuntimeUnavailable => "runtime_unavailable",
            ErrorCode::Internal => "internal",
        }
    }

    /// The HTTP status this code is always returned with.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            ErrorCode::InvalidRequest
            | ErrorCode::InvalidGoal
            | ErrorCode::InvalidAnswer
            | ErrorCode::InvalidCursor => 400,
            ErrorCode::PayloadTooLarge => 413,
            ErrorCode::Unauthenticated => 401,
            ErrorCode::Forbidden => 403,
            ErrorCode::RunNotFound
            | ErrorCode::TaskNotFound
            | ErrorCode::QuestionNotFound
            | ErrorCode::ProposalNotFound
            | ErrorCode::ArtifactNotFound => 404,
            ErrorCode::IdempotencyConflict
            | ErrorCode::RunNotInState
            | ErrorCode::TaskNotInState
            | ErrorCode::QuestionAlreadyAnswered
            | ErrorCode::VersionConflict => 409,
            ErrorCode::PlanInvalid
            | ErrorCode::BudgetInvalid
            | ErrorCode::UnknownModelAlias
            | ErrorCode::WorkerDisabled => 422,
            ErrorCode::RateLimited => 429,
            ErrorCode::Draining | ErrorCode::DbUnavailable | ErrorCode::RuntimeUnavailable => 503,
            ErrorCode::Internal => 500,
        }
    }

    /// Every code, for the OpenAPI document and the code-table test.
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::InvalidRequest,
        ErrorCode::InvalidGoal,
        ErrorCode::InvalidAnswer,
        ErrorCode::InvalidCursor,
        ErrorCode::PayloadTooLarge,
        ErrorCode::Unauthenticated,
        ErrorCode::Forbidden,
        ErrorCode::RunNotFound,
        ErrorCode::TaskNotFound,
        ErrorCode::QuestionNotFound,
        ErrorCode::ProposalNotFound,
        ErrorCode::ArtifactNotFound,
        ErrorCode::IdempotencyConflict,
        ErrorCode::RunNotInState,
        ErrorCode::TaskNotInState,
        ErrorCode::QuestionAlreadyAnswered,
        ErrorCode::VersionConflict,
        ErrorCode::PlanInvalid,
        ErrorCode::BudgetInvalid,
        ErrorCode::UnknownModelAlias,
        ErrorCode::WorkerDisabled,
        ErrorCode::RateLimited,
        ErrorCode::Draining,
        ErrorCode::DbUnavailable,
        ErrorCode::RuntimeUnavailable,
        ErrorCode::Internal,
    ];
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The JSON body of every non-2xx response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(utoipa::ToSchema))]
pub struct ErrorBody {
    /// Stable, language-neutral code (see [`ErrorCode`]).
    pub code: String,
    /// Human-readable, redacted explanation.
    pub message: String,
    /// Structured context (`{"run_id": "…"}`), when there is any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "server", schema(value_type = Object))]
    pub details: Option<Value>,
    /// The `x-request-id` of the failing request.
    pub request_id: String,
}

/// An API failure: a code, a message and optional details.
///
/// The status is derived from the code, so a handler can never return a code
/// with the wrong status.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// The stable code.
    pub code: ErrorCode,
    /// Human-readable, redacted explanation.
    pub message: String,
    /// Structured context.
    pub details: Option<Value>,
}

impl ApiError {
    /// An error with a code and a message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Adds structured details.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// The HTTP status of this error.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.code.status()
    }

    /// The wire body, stamped with `request_id`.
    #[must_use]
    pub fn body(&self, request_id: &str) -> ErrorBody {
        ErrorBody {
            code: self.code.as_str().to_owned(),
            message: self.message.clone(),
            details: self.details.clone(),
            request_id: request_id.to_owned(),
        }
    }

    // -- shorthands used across the handlers --------------------------------

    /// `400 invalid_request`.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    /// `404 run_not_found`.
    #[must_use]
    pub fn run_not_found(run_id: uuid::Uuid) -> Self {
        Self::new(
            ErrorCode::RunNotFound,
            format!("run {run_id} does not exist"),
        )
        .with_details(serde_json::json!({ "run_id": run_id.to_string() }))
    }

    /// `404 task_not_found`.
    #[must_use]
    pub fn task_not_found(task_id: uuid::Uuid) -> Self {
        Self::new(
            ErrorCode::TaskNotFound,
            format!("task {task_id} does not exist"),
        )
        .with_details(serde_json::json!({ "task_id": task_id.to_string() }))
    }

    /// `404 question_not_found`.
    #[must_use]
    pub fn question_not_found(question_id: uuid::Uuid) -> Self {
        Self::new(
            ErrorCode::QuestionNotFound,
            format!("question {question_id} does not exist"),
        )
        .with_details(serde_json::json!({ "question_id": question_id.to_string() }))
    }

    /// `503 runtime_unavailable` — the orchestrator port is not wired up.
    pub fn runtime_unavailable(what: impl std::fmt::Display) -> Self {
        Self::new(
            ErrorCode::RuntimeUnavailable,
            format!("{what} is not available on this deployment"),
        )
    }

    /// `500 internal`; the cause is logged, never returned.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

#[cfg(feature = "server")]
mod server {
    use axum::Json;
    use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use kevin_domain::DomainError;
    use kevin_orchestrator::projections::ProjectionError;
    use kevin_store::StoreError;

    use super::{ApiError, ErrorCode};
    use crate::request_id::RequestId;

    impl ApiError {
        /// The axum status of this error.
        pub(crate) fn http_status(&self) -> StatusCode {
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }

    impl IntoResponse for ApiError {
        fn into_response(self) -> Response {
            // The request id is injected by the middleware; when it is missing
            // (a rejection before the middleware ran) the envelope still has
            // the field, empty, so clients can parse it unconditionally.
            let request_id = RequestId::current().unwrap_or_default();
            let status = self.http_status();
            if status.is_server_error() {
                tracing::error!(
                    event = "kevin.api.request",
                    code = self.code.as_str(),
                    message = %self.message,
                    "api error"
                );
            }
            let mut response = (status, Json(self.body(&request_id))).into_response();
            response.extensions_mut().insert(self.code);
            response
        }
    }

    impl From<ProjectionError> for ApiError {
        fn from(err: ProjectionError) -> Self {
            match err {
                ProjectionError::InvalidCursor { cursor } => ApiError::new(
                    ErrorCode::InvalidCursor,
                    format!("invalid cursor {cursor:?}"),
                ),
                ProjectionError::Db(source) => {
                    tracing::error!(error = %source, "read model query failed");
                    ApiError::new(ErrorCode::DbUnavailable, "the read models are unavailable")
                }
                other => {
                    tracing::error!(error = %other, "read model failure");
                    ApiError::internal("read model failure")
                }
            }
        }
    }

    impl From<StoreError> for ApiError {
        fn from(err: StoreError) -> Self {
            match err {
                StoreError::VersionConflict { .. } => ApiError::new(
                    ErrorCode::VersionConflict,
                    "the aggregate changed concurrently; re-read and retry",
                ),
                other => {
                    tracing::error!(error = %other, "event store failure");
                    ApiError::new(ErrorCode::DbUnavailable, "the event store is unavailable")
                }
            }
        }
    }

    impl From<DomainError> for ApiError {
        fn from(err: DomainError) -> Self {
            let message = err.to_string();
            let code = match err {
                DomainError::InvalidTransition { aggregate, .. }
                | DomainError::AlreadyExists { aggregate, .. } => {
                    if aggregate == "task" {
                        ErrorCode::TaskNotInState
                    } else {
                        ErrorCode::RunNotInState
                    }
                }
                DomainError::AttemptsExhausted { .. }
                | DomainError::NotRetryable { .. }
                | DomainError::AttemptAlreadyRunning { .. }
                | DomainError::AttemptMismatch { .. }
                | DomainError::RouteRequired => ErrorCode::TaskNotInState,
                DomainError::PlanRequired | DomainError::ProposalAlreadyDecided { .. } => {
                    ErrorCode::RunNotInState
                }
                DomainError::AlreadyAnswered => ErrorCode::QuestionAlreadyAnswered,
                DomainError::QuestionDoesNotExpire | DomainError::InvalidAnswer { .. } => {
                    ErrorCode::InvalidAnswer
                }
                DomainError::InvalidPlan(_) => ErrorCode::PlanInvalid,
                DomainError::BudgetExhausted { .. } => ErrorCode::BudgetInvalid,
                DomainError::UnknownProposal { .. } => ErrorCode::ProposalNotFound,
                DomainError::UnknownQuestion { .. } => ErrorCode::QuestionNotFound,
                DomainError::UnknownTask { .. } => ErrorCode::TaskNotFound,
                DomainError::NotFound { aggregate, .. } => match aggregate {
                    "task" => ErrorCode::TaskNotFound,
                    "question" => ErrorCode::QuestionNotFound,
                    _ => ErrorCode::RunNotFound,
                },
                DomainError::InvalidValue(_) => ErrorCode::InvalidRequest,
            };
            ApiError::new(code, message)
        }
    }

    impl From<JsonRejection> for ApiError {
        fn from(rejection: JsonRejection) -> Self {
            match rejection {
                JsonRejection::BytesRejection(_) => {
                    ApiError::new(ErrorCode::PayloadTooLarge, "request body is too large")
                }
                other => ApiError::invalid_request(other.body_text()),
            }
        }
    }

    impl From<QueryRejection> for ApiError {
        fn from(rejection: QueryRejection) -> Self {
            ApiError::invalid_request(rejection.body_text())
        }
    }

    impl From<PathRejection> for ApiError {
        fn from(rejection: PathRejection) -> Self {
            ApiError::invalid_request(rejection.body_text())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, ErrorCode};

    #[test]
    fn every_code_has_the_status_from_the_plan_table() {
        // Spot-checks of the table in plan/07 §Conventions.
        assert_eq!(ErrorCode::InvalidGoal.status(), 400);
        assert_eq!(ErrorCode::Unauthenticated.status(), 401);
        assert_eq!(ErrorCode::Forbidden.status(), 403);
        assert_eq!(ErrorCode::RunNotFound.status(), 404);
        assert_eq!(ErrorCode::IdempotencyConflict.status(), 409);
        assert_eq!(ErrorCode::PlanInvalid.status(), 422);
        assert_eq!(ErrorCode::RateLimited.status(), 429);
        assert_eq!(ErrorCode::Draining.status(), 503);
        assert_eq!(ErrorCode::Internal.status(), 500);
    }

    #[test]
    fn codes_are_unique_and_snake_case() {
        let mut names: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate error code");
        assert!(
            names
                .iter()
                .all(|n| n.chars().all(|c| c.is_ascii_lowercase() || c == '_')),
            "codes are snake_case"
        );
    }

    #[test]
    fn the_envelope_carries_the_request_id() {
        let body = ApiError::run_not_found(uuid::Uuid::nil()).body("req-1");
        assert_eq!(body.code, "run_not_found");
        assert_eq!(body.request_id, "req-1");
        assert!(body.details.is_some());
    }
}
