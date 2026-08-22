//! The boundary between the HTTP layer and the runtime.
//!
//! The API is deliberately written against **narrow ports** instead of against
//! concrete orchestrator/router/evaluator types:
//!
//! - it keeps the handlers testable without Postgres or a worker subprocess
//!   (`kevin_testkit::fake_api`);
//! - it documents, in one place, exactly which runtime capabilities the HTTP
//!   surface needs, so a new backend (Kohral, an embedded runtime) implements
//!   a short list of traits instead of reading every handler;
//! - it keeps the compile-time dependency one-way: nothing below the API has
//!   to know an HTTP layer exists.
//!
//! Production implementations: [`RuntimePort`] → [`crate::runtime`] over the
//! orchestrator's services; the read-side ports → [`crate::adapters`] over the
//! projections, the store, the bus, the worker registry and the memory store.
//! [`EvaluatorPort`] has no adapter yet — `kevin-evaluator` (WS-19) is still a
//! stub, so the proposals endpoints answer `503 runtime_unavailable` until it
//! lands.
//!
//! Ports return **DTOs**, not aggregates: a write returns the state the caller
//! should see *now*, built from the aggregate itself, so a client never reads
//! a stale projection right after a successful command.

use std::fmt;

use async_trait::async_trait;
use kevin_bus::BusStream;
use kevin_domain::ids::{
    ArtifactId, CommandId, MemoryItemId, ProposalId, QuestionId, RunId, TaskId,
};
use kevin_domain::{Actor, DomainError};
use uuid::Uuid;

use crate::dto::{
    AnswerRequest, ArtifactDto, CostQueryDto, CostReportDto, CreateRunRequest, DrainStatusDto,
    EventDto, LessonsQuery, ListRunsQuery, MemoryItemDto, MemorySearchQuery, Page, ProposalDto,
    ProposalsQuery, QuestionDto, QuestionsQuery, RouteScoreDto, RunDto, RunSummaryDto, TaskDto,
    TaskLogLineDto, TaskLogQueryDto, WorkerDoctorDto,
};
use crate::error::{ApiError, ErrorCode};

/// Result of every port call.
pub type PortResult<T> = Result<T, RuntimeError>;

// ---------------------------------------------------------------------------
// Command context
// ---------------------------------------------------------------------------

/// Everything a command needs beyond its own arguments.
#[derive(Debug, Clone)]
pub struct CommandCtx {
    /// Idempotency key of the command; `core.processed_commands` deduplicates
    /// on it, so a replay returns the original result (plan/07 §Conventions).
    pub command_id: CommandId,
    /// The `x-request-id`, when it parses as a uuid; becomes `causation_id`.
    pub causation_id: Option<Uuid>,
    /// Who asked.
    pub actor: Actor,
}

impl CommandCtx {
    /// A context with a fresh command id and no causation.
    #[must_use]
    pub fn new(actor: Actor) -> Self {
        Self {
            command_id: CommandId::new(),
            causation_id: None,
            actor,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What a port call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// An aggregate refused the command.
    #[error(transparent)]
    Domain(#[from] DomainError),

    /// No run with that id.
    #[error("run {0} does not exist")]
    RunNotFound(RunId),

    /// No task with that id.
    #[error("task {0} does not exist")]
    TaskNotFound(TaskId),

    /// No question with that id.
    #[error("question {0} does not exist")]
    QuestionNotFound(QuestionId),

    /// No proposal with that id.
    #[error("proposal {0} does not exist")]
    ProposalNotFound(ProposalId),

    /// No artifact with that id.
    #[error("artifact {0} does not exist")]
    ArtifactNotFound(ArtifactId),

    /// The same `Idempotency-Key` was replayed with a different body.
    #[error("idempotency key `{key}` was already used with a different request")]
    IdempotencyConflict {
        /// The offending key.
        key: String,
    },

    /// Admission is closed; the runtime refuses new work.
    #[error("the runtime is draining and does not accept new runs")]
    Draining,

    /// A dependency the API needs is not wired up on this deployment.
    #[error("{0} is not available on this deployment")]
    Unavailable(String),

    /// The store or the read models did not answer.
    #[error("storage unavailable: {0}")]
    Storage(String),

    /// Anything else; logged, never echoed verbatim.
    #[error("{0}")]
    Internal(String),
}

impl RuntimeError {
    /// Shorthand for [`RuntimeError::Unavailable`].
    pub fn unavailable(what: impl Into<String>) -> Self {
        RuntimeError::Unavailable(what.into())
    }

    /// Shorthand for [`RuntimeError::Internal`].
    pub fn internal(what: impl fmt::Display) -> Self {
        RuntimeError::Internal(what.to_string())
    }
}

impl From<RuntimeError> for ApiError {
    fn from(err: RuntimeError) -> Self {
        match err {
            RuntimeError::Domain(domain) => domain.into(),
            RuntimeError::RunNotFound(id) => ApiError::run_not_found(id.as_uuid()),
            RuntimeError::TaskNotFound(id) => ApiError::task_not_found(id.as_uuid()),
            RuntimeError::QuestionNotFound(id) => ApiError::question_not_found(id.as_uuid()),
            RuntimeError::ProposalNotFound(id) => ApiError::new(
                ErrorCode::ProposalNotFound,
                format!("proposal {id} does not exist"),
            ),
            RuntimeError::ArtifactNotFound(id) => ApiError::new(
                ErrorCode::ArtifactNotFound,
                format!("artifact {id} does not exist"),
            ),
            RuntimeError::IdempotencyConflict { key } => ApiError::new(
                ErrorCode::IdempotencyConflict,
                "this Idempotency-Key was already used with a different request body",
            )
            .with_details(serde_json::json!({ "idempotency_key": key })),
            RuntimeError::Draining => ApiError::new(
                ErrorCode::Draining,
                "the runtime is draining and does not accept new runs",
            ),
            RuntimeError::Unavailable(what) => ApiError::runtime_unavailable(what),
            RuntimeError::Storage(message) => {
                tracing::error!(error = %message, "storage failure");
                ApiError::new(ErrorCode::DbUnavailable, "storage is unavailable")
            }
            RuntimeError::Internal(message) => {
                tracing::error!(error = %message, "runtime failure");
                ApiError::internal("internal error")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RuntimePort — the write side
// ---------------------------------------------------------------------------

/// Readiness of the runtime (`GET /readyz`, plan/10 §Health and drain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Readiness {
    /// The database answered a ping within the deadline.
    pub db: bool,
    /// Admission is closed.
    pub draining: bool,
    /// Every enabled worker passed `doctor`.
    pub workers_ok: bool,
}

impl Readiness {
    /// Ready when the db answers, the workers are healthy and we are not draining.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.db && self.workers_ok && !self.draining
    }
}

/// Every **write** the HTTP API performs, plus admission control.
///
/// Implemented by [`crate::runtime::OrchestratorRuntime`] over WS-08's
/// `RunService`/`TaskService`/`QuestionService`, and by
/// `kevin_testkit::fake_api::FakeRuntime`.
#[async_trait]
pub trait RuntimePort: Send + Sync + fmt::Debug {
    /// `POST /api/v1/runs` → `StartRun`.
    async fn start_run(&self, request: CreateRunRequest, ctx: CommandCtx) -> PortResult<RunDto>;

    /// `POST /api/v1/runs/{run_id}/cancel` → `CancelRun`.
    async fn cancel_run(
        &self,
        run_id: RunId,
        reason: Option<String>,
        ctx: CommandCtx,
    ) -> PortResult<RunDto>;

    /// `POST /api/v1/runs/{run_id}/plan/approve` → `ApprovePlan`.
    async fn approve_plan(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<RunDto>;

    /// `POST /api/v1/runs/{run_id}/plan/reject` → `RejectPlan`.
    async fn reject_plan(
        &self,
        run_id: RunId,
        feedback: String,
        ctx: CommandCtx,
    ) -> PortResult<RunDto>;

    /// `POST /api/v1/runs/{run_id}/evaluate` → re-run the judge.
    async fn evaluate_run(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<()>;

    /// `POST /api/v1/tasks/{task_id}/retry` → `RetryTask`.
    async fn retry_task(
        &self,
        task_id: TaskId,
        exclude_route: bool,
        ctx: CommandCtx,
    ) -> PortResult<TaskDto>;

    /// `POST /api/v1/tasks/{task_id}/cancel` → `CancelTask`.
    async fn cancel_task(&self, task_id: TaskId, ctx: CommandCtx) -> PortResult<TaskDto>;

    /// `POST /api/v1/questions/{question_id}/answer` → `AnswerQuestion`.
    async fn answer_question(
        &self,
        question_id: QuestionId,
        answer: AnswerRequest,
        ctx: CommandCtx,
    ) -> PortResult<QuestionDto>;

    /// `POST|DELETE /api/v1/maintenance/drain`: open or close admission.
    async fn set_drain(&self, draining: bool) -> PortResult<DrainStatusDto>;

    /// `GET /api/v1/maintenance/drain`.
    async fn drain_status(&self) -> PortResult<DrainStatusDto>;

    /// `GET /readyz`. Never fails: an unreachable dependency is reported as
    /// "not ready", not as an error.
    async fn readiness(&self) -> Readiness;
}

// ---------------------------------------------------------------------------
// ReadPort — the projections
// ---------------------------------------------------------------------------

/// Every **read** the HTTP API performs against the `orch.*` read models.
///
/// Implemented by [`crate::adapters::ProjectionReads`] over
/// `kevin_orchestrator::projections::ReadModels`.
#[async_trait]
pub trait ReadPort: Send + Sync + fmt::Debug {
    /// `GET /api/v1/runs/{run_id}`.
    async fn run(&self, run_id: RunId) -> PortResult<Option<RunDto>>;

    /// `GET /api/v1/runs`.
    async fn runs(&self, query: &ListRunsQuery) -> PortResult<Page<RunSummaryDto>>;

    /// `GET /api/v1/runs/{run_id}/tasks`.
    async fn tasks_of_run(&self, run_id: RunId) -> PortResult<Vec<TaskDto>>;

    /// `GET /api/v1/tasks/{task_id}`.
    async fn task(&self, task_id: TaskId) -> PortResult<Option<TaskDto>>;

    /// `GET /api/v1/tasks/{task_id}/log`.
    async fn task_log(
        &self,
        task_id: TaskId,
        query: &TaskLogQueryDto,
    ) -> PortResult<Page<TaskLogLineDto>>;

    /// `GET /api/v1/tasks/{task_id}/artifacts`.
    async fn artifacts_of_task(&self, task_id: TaskId) -> PortResult<Vec<ArtifactDto>>;

    /// `GET /api/v1/artifacts/{artifact_id}` (metadata; the bytes come from
    /// [`ArtifactsPort`]).
    async fn artifact(&self, artifact_id: ArtifactId) -> PortResult<Option<ArtifactDto>>;

    /// `GET /api/v1/questions/{question_id}`.
    async fn question(&self, question_id: QuestionId) -> PortResult<Option<QuestionDto>>;

    /// `GET /api/v1/questions`.
    async fn questions(&self, query: &QuestionsQuery) -> PortResult<Page<QuestionDto>>;

    /// `GET /api/v1/cost`.
    async fn cost(&self, query: &CostQueryDto) -> PortResult<CostReportDto>;
}

// ---------------------------------------------------------------------------
// EventsPort — SSE catch-up + live
// ---------------------------------------------------------------------------

/// History and live fan-out behind the three SSE endpoints.
#[async_trait]
pub trait EventsPort: Send + Sync + fmt::Debug {
    /// Committed events with `position > from`, at most `limit`, in position
    /// order (the `Last-Event-ID` catch-up).
    async fn after(&self, from: u64, limit: usize) -> PortResult<Vec<EventDto>>;

    /// A **live-only** subscription: events fanned out from now on. History is
    /// always read through [`EventsPort::after`], so the catch-up and the live
    /// tail have one source of truth each and the seam guard is exact.
    fn subscribe_live(&self) -> BusStream;

    /// Highest position the bus has fanned out (`0` = none yet).
    fn head(&self) -> u64;
}

// ---------------------------------------------------------------------------
// Side ports
// ---------------------------------------------------------------------------

/// `GET /api/v1/routes` — the routing leaderboard (`kevin-router`, WS-14).
#[async_trait]
pub trait RouterPort: Send + Sync + fmt::Debug {
    /// Scores, best first, optionally restricted to one task kind.
    async fn leaderboard(&self, kind: Option<&str>) -> PortResult<Vec<RouteScoreDto>>;
}

/// `GET /api/v1/proposals` and the accept/reject verbs (`kevin-evaluator`, WS-19).
#[async_trait]
pub trait EvaluatorPort: Send + Sync + fmt::Debug {
    /// The proposals inbox.
    async fn proposals(&self, query: &ProposalsQuery) -> PortResult<Page<ProposalDto>>;

    /// Accepts (`accept = true`) or rejects a proposal.
    async fn decide_proposal(
        &self,
        proposal_id: ProposalId,
        accept: bool,
        note: Option<String>,
        ctx: CommandCtx,
    ) -> PortResult<ProposalDto>;
}

/// `GET /api/v1/memory/search`, `GET /api/v1/lessons`, `DELETE /api/v1/memory/{id}`.
#[async_trait]
pub trait MemoryPort: Send + Sync + fmt::Debug {
    /// Hybrid search over the memory items.
    async fn search(&self, query: &MemorySearchQuery) -> PortResult<Vec<MemoryItemDto>>;

    /// The lessons view, paginated.
    async fn lessons(&self, query: &LessonsQuery) -> PortResult<Page<MemoryItemDto>>;

    /// `ForgetMemoryItem`.
    async fn forget(&self, item_id: MemoryItemId, actor: Actor) -> PortResult<()>;
}

/// `GET /api/v1/workers` — `Worker::doctor()` for every configured worker.
#[async_trait]
pub trait WorkersPort: Send + Sync + fmt::Debug {
    /// One row per worker kind.
    async fn doctor(&self) -> PortResult<Vec<WorkerDoctorDto>>;
}

/// `GET /api/v1/artifacts/{artifact_id}` — the bytes behind an artifact.
#[async_trait]
pub trait ArtifactsPort: Send + Sync + fmt::Debug {
    /// Content type and bytes of `artifact`.
    async fn read(&self, artifact: &ArtifactDto) -> PortResult<(String, Vec<u8>)>;
}
