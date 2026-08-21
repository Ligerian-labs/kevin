//! The [`Task`] aggregate (`plan/02-domain-model.md` §Aggregates › Task).
//!
//! ```text
//! pending ──RouteTask──▶ routed ──StartAttempt──▶ running
//! routed ──RouteTask──▶ routed                       (re-route before start / after retry)
//! running ──SucceedAttempt──▶ succeeded
//! running|awaiting_input ──FailAttempt──▶ failed ──RetryTask (attempts < max, retryable class)──▶ routed
//! running|awaiting_input ──FailAttempt{class: cancelled}──▶ cancelled
//! running ──RequestInput──▶ awaiting_input ──ProvideInput──▶ running
//! pending|routed|running|awaiting_input|failed(retryable) ──CancelTask──▶ cancelled
//! pending ──SkipTask──▶ skipped
//! ```
//!
//! Invariants: one active attempt at a time; `StartAttempt` requires a route;
//! total attempts ≤ `budget.max_attempts`; `failed` is terminal unless
//! [`Task::can_retry`] (the saga decides whether to retry).

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::ids::{AttemptId, QuestionId, RunId, TaskId};
use crate::kinds::{FailureClass, ModelAlias, TaskKind};
use crate::values::{ArtifactRef, Budget, Route, TaskSpec, Usage, Workspace};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const TASK_AGGREGATE_TYPE: &str = "task";

// ---------------------------------------------------------------------------
// Status, attempts, routing info
// ---------------------------------------------------------------------------

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Created, waiting for dependencies / routing.
    #[default]
    Pending,
    /// Has a route, waiting for a permit.
    Routed,
    /// An attempt is in flight.
    Running,
    /// The attempt asked a question and is paused.
    AwaitingInput,
    /// Terminal: an attempt succeeded.
    Succeeded,
    /// The last attempt failed; terminal unless a retry is still possible.
    Failed,
    /// Terminal: cancelled.
    Cancelled,
    /// Terminal: skipped (dependency failed).
    Skipped,
}

impl TaskStatus {
    /// Every status.
    pub const ALL: [TaskStatus; 8] = [
        TaskStatus::Pending,
        TaskStatus::Routed,
        TaskStatus::Running,
        TaskStatus::AwaitingInput,
        TaskStatus::Succeeded,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
        TaskStatus::Skipped,
    ];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Routed => "routed",
            TaskStatus::Running => "running",
            TaskStatus::AwaitingInput => "awaiting_input",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Skipped => "skipped",
        }
    }

    /// `running` or `awaiting_input`.
    #[must_use]
    pub const fn has_active_attempt(self) -> bool {
        matches!(self, TaskStatus::Running | TaskStatus::AwaitingInput)
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    /// Worker is running.
    Running,
    /// Paused on a question.
    AwaitingInput,
    /// Finished successfully.
    Succeeded,
    /// Failed (see `failure`).
    Failed,
}

/// Why an attempt failed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptFailure {
    /// Class.
    pub class: FailureClass,
    /// Message.
    pub message: String,
}

/// One worker attempt on a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    /// Attempt id.
    pub id: AttemptId,
    /// 1-based attempt number.
    pub no: u8,
    /// Route used.
    pub route: Route,
    /// Workspace the worker ran in.
    pub workspace: Workspace,
    /// Worker session id (for resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_session_id: Option<String>,
    /// State.
    pub status: AttemptStatus,
    /// Usage (accumulated from progress, replaced by the terminal total).
    pub usage: Usage,
    /// Last progress / final summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Failure detail when `status == Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<AttemptFailure>,
    /// Question the attempt is waiting on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_question: Option<QuestionId>,
    /// Highest `log_seq` seen in progress events.
    #[serde(default)]
    pub last_log_seq: u64,
}

impl Attempt {
    /// `running` or `awaiting_input`.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(
            self.status,
            AttemptStatus::Running | AttemptStatus::AwaitingInput
        )
    }
}

/// Routing policy that produced a selection (`plan/06-memory-and-learning.md` §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    /// Thompson sampling over Beta posteriors.
    Thompson,
    /// ε-greedy on win rate.
    EpsilonGreedy,
    /// First configured candidate.
    Fixed,
    /// No candidates: `[roles].default`.
    Fallback,
}

/// Why a route was chosen (`task.routed.selection`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteSelectionInfo {
    /// Policy used.
    pub policy: RoutingPolicy,
    /// Candidates considered, in evaluation order.
    #[serde(default)]
    pub candidates: Vec<ModelAlias>,
    /// Score per candidate (same order as `candidates`).
    #[serde(default)]
    pub scores: Vec<f32>,
    /// Catalog version the scores refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_version: Option<String>,
}

impl RouteSelectionInfo {
    /// A fixed-policy selection with a single candidate.
    #[must_use]
    pub fn fixed(alias: ModelAlias) -> Self {
        Self {
            policy: RoutingPolicy::Fixed,
            candidates: vec![alias],
            scores: vec![1.0],
            catalog_version: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Creates the task (`task.created`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTask {
    /// New task id.
    pub task_id: TaskId,
    /// Owning run.
    pub run_id: RunId,
    /// Kind.
    pub kind: TaskKind,
    /// Spec.
    pub spec: TaskSpec,
    /// Task budget (`max_attempts` bounds retries).
    pub budget: Budget,
}

/// Assigns a route (`task.routed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteTask {
    /// The route.
    pub route: Route,
    /// How it was chosen.
    pub selection: RouteSelectionInfo,
}

/// Starts an attempt on the routed task (`task.attempt_started`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartAttempt {
    /// New attempt id.
    pub attempt_id: AttemptId,
    /// Prepared workspace.
    pub workspace: Workspace,
    /// Worker session id when known up front.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_session_id: Option<String>,
}

/// Throttled progress from the runner (`task.progressed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordProgress {
    /// Active attempt.
    pub attempt_id: AttemptId,
    /// Short summary of what the worker is doing.
    pub summary: String,
    /// Usage since the last progress event.
    pub usage_delta: Usage,
    /// `orch.task_log.seq` of the last folded worker event.
    pub log_seq: u64,
}

/// The worker asked a question (`task.input_requested`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestInput {
    /// Active attempt.
    pub attempt_id: AttemptId,
    /// The question created for it.
    pub question_id: QuestionId,
}

/// The question was answered and the worker resumed (`task.input_provided`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvideInput {
    /// Active attempt.
    pub attempt_id: AttemptId,
    /// The answered question.
    pub question_id: QuestionId,
}

/// The attempt finished successfully (`task.attempt_succeeded`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SucceedAttempt {
    /// Active attempt.
    pub attempt_id: AttemptId,
    /// Produced artifacts.
    pub artifacts: Vec<ArtifactRef>,
    /// Total usage of the attempt (zero = keep the accumulated progress usage).
    pub usage: Usage,
    /// Final summary.
    pub summary: String,
}

/// The attempt failed (`task.attempt_failed`, plus `task.cancelled` for
/// `class == Cancelled`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailAttempt {
    /// Active attempt.
    pub attempt_id: AttemptId,
    /// Class.
    pub class: FailureClass,
    /// Message.
    pub message: String,
    /// Total usage of the attempt (zero = keep the accumulated progress usage).
    pub usage: Usage,
}

/// Allows another attempt after a retryable failure (`task.retried`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryTask {
    /// Why.
    pub reason: String,
}

/// Cancels the task (`task.cancelled`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelTask {
    /// Why.
    pub reason: String,
}

/// Skips a pending task (`task.skipped`), e.g. `dependency_failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipTask {
    /// Why.
    pub reason: String,
}

/// Every command the [`Task`] aggregate handles.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskCommand {
    /// [`CreateTask`].
    Create(CreateTask),
    /// [`RouteTask`].
    Route(RouteTask),
    /// [`StartAttempt`].
    StartAttempt(StartAttempt),
    /// [`RecordProgress`].
    RecordProgress(RecordProgress),
    /// [`RequestInput`].
    RequestInput(RequestInput),
    /// [`ProvideInput`].
    ProvideInput(ProvideInput),
    /// [`SucceedAttempt`].
    SucceedAttempt(SucceedAttempt),
    /// [`FailAttempt`].
    FailAttempt(FailAttempt),
    /// [`RetryTask`].
    Retry(RetryTask),
    /// [`CancelTask`].
    Cancel(CancelTask),
    /// [`SkipTask`].
    Skip(SkipTask),
}

impl TaskCommand {
    /// `snake_case` command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            TaskCommand::Create(_) => "create_task",
            TaskCommand::Route(_) => "route_task",
            TaskCommand::StartAttempt(_) => "start_attempt",
            TaskCommand::RecordProgress(_) => "record_progress",
            TaskCommand::RequestInput(_) => "request_input",
            TaskCommand::ProvideInput(_) => "provide_input",
            TaskCommand::SucceedAttempt(_) => "succeed_attempt",
            TaskCommand::FailAttempt(_) => "fail_attempt",
            TaskCommand::Retry(_) => "retry_task",
            TaskCommand::Cancel(_) => "cancel_task",
            TaskCommand::Skip(_) => "skip_task",
        }
    }
}

macro_rules! command_from {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        $(impl From<$ty> for TaskCommand {
            fn from(cmd: $ty) -> Self {
                TaskCommand::$variant(cmd)
            }
        })*
    };
}

command_from!(
    Create(CreateTask),
    Route(RouteTask),
    StartAttempt(StartAttempt),
    RecordProgress(RecordProgress),
    RequestInput(RequestInput),
    ProvideInput(ProvideInput),
    SucceedAttempt(SucceedAttempt),
    FailAttempt(FailAttempt),
    Retry(RetryTask),
    Cancel(CancelTask),
    Skip(SkipTask),
);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `task` stream (internally tagged on `type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TaskEvent {
    /// `task.created`
    #[serde(rename = "task.created")]
    Created {
        /// Task id.
        task_id: TaskId,
        /// Owning run.
        run_id: RunId,
        /// Kind.
        kind: TaskKind,
        /// Spec.
        spec: TaskSpec,
        /// Budget.
        budget: Budget,
    },
    /// `task.routed`
    #[serde(rename = "task.routed")]
    Routed {
        /// Route.
        route: Route,
        /// Selection detail.
        selection: RouteSelectionInfo,
    },
    /// `task.attempt_started`
    #[serde(rename = "task.attempt_started")]
    AttemptStarted {
        /// Attempt id.
        attempt_id: AttemptId,
        /// 1-based attempt number.
        attempt_no: u8,
        /// Route used.
        route: Route,
        /// Workspace.
        workspace: Workspace,
        /// Worker session id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_session_id: Option<String>,
    },
    /// `task.progressed`
    #[serde(rename = "task.progressed")]
    Progressed {
        /// Attempt.
        attempt_id: AttemptId,
        /// Summary.
        summary: String,
        /// Usage since last progress.
        usage_delta: Usage,
        /// Task log sequence.
        log_seq: u64,
    },
    /// `task.input_requested`
    #[serde(rename = "task.input_requested")]
    InputRequested {
        /// Attempt.
        attempt_id: AttemptId,
        /// Question.
        question_id: QuestionId,
    },
    /// `task.input_provided`
    #[serde(rename = "task.input_provided")]
    InputProvided {
        /// Attempt.
        attempt_id: AttemptId,
        /// Question.
        question_id: QuestionId,
    },
    /// `task.attempt_succeeded`
    #[serde(rename = "task.attempt_succeeded")]
    AttemptSucceeded {
        /// Attempt.
        attempt_id: AttemptId,
        /// Artifacts.
        artifacts: Vec<ArtifactRef>,
        /// Summary.
        summary: String,
        /// Final attempt usage.
        usage: Usage,
    },
    /// `task.attempt_failed`
    #[serde(rename = "task.attempt_failed")]
    AttemptFailed {
        /// Attempt.
        attempt_id: AttemptId,
        /// Class.
        class: FailureClass,
        /// Message.
        message: String,
        /// Final attempt usage.
        usage: Usage,
        /// Another attempt is allowed (retryable class and attempts < max).
        retry_possible: bool,
    },
    /// `task.retried`
    #[serde(rename = "task.retried")]
    Retried {
        /// Number the next attempt will get.
        next_attempt_no: u8,
        /// Why.
        reason: String,
    },
    /// `task.cancelled`
    #[serde(rename = "task.cancelled")]
    Cancelled {
        /// Why.
        reason: String,
    },
    /// `task.skipped`
    #[serde(rename = "task.skipped")]
    Skipped {
        /// Why.
        reason: String,
    },
}

impl TaskEvent {
    /// Every event type of the `task` stream, in catalog order.
    pub const TYPES: [&'static str; 11] = [
        "task.created",
        "task.routed",
        "task.attempt_started",
        "task.progressed",
        "task.input_requested",
        "task.input_provided",
        "task.attempt_succeeded",
        "task.attempt_failed",
        "task.retried",
        "task.cancelled",
        "task.skipped",
    ];
}

impl EventMeta for TaskEvent {
    fn event_type(&self) -> &'static str {
        match self {
            TaskEvent::Created { .. } => "task.created",
            TaskEvent::Routed { .. } => "task.routed",
            TaskEvent::AttemptStarted { .. } => "task.attempt_started",
            TaskEvent::Progressed { .. } => "task.progressed",
            TaskEvent::InputRequested { .. } => "task.input_requested",
            TaskEvent::InputProvided { .. } => "task.input_provided",
            TaskEvent::AttemptSucceeded { .. } => "task.attempt_succeeded",
            TaskEvent::AttemptFailed { .. } => "task.attempt_failed",
            TaskEvent::Retried { .. } => "task.retried",
            TaskEvent::Cancelled { .. } => "task.cancelled",
            TaskEvent::Skipped { .. } => "task.skipped",
        }
    }

    fn schema_version(&self) -> u16 {
        1
    }

    fn aggregate_type(&self) -> &'static str {
        TASK_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The task aggregate.
#[derive(Debug, Clone)]
pub struct Task {
    version: u64,
    id: TaskId,
    run_id: RunId,
    kind: Option<TaskKind>,
    spec: Option<TaskSpec>,
    route: Option<Route>,
    attempts: Vec<Attempt>,
    status: TaskStatus,
    budget: Budget,
    usage: Usage,
    artifacts: Vec<ArtifactRef>,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            version: 0,
            id: TaskId::nil(),
            run_id: RunId::nil(),
            kind: None,
            spec: None,
            route: None,
            attempts: Vec::new(),
            status: TaskStatus::Pending,
            budget: Budget::unlimited(),
            usage: Usage::ZERO,
            artifacts: Vec::new(),
        }
    }
}

impl Task {
    /// Typed id.
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.id
    }

    /// Owning run.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    /// Kind (after `task.created`).
    #[must_use]
    pub const fn kind(&self) -> Option<&TaskKind> {
        self.kind.as_ref()
    }

    /// Spec (after `task.created`).
    #[must_use]
    pub const fn spec(&self) -> Option<&TaskSpec> {
        self.spec.as_ref()
    }

    /// Current route.
    #[must_use]
    pub const fn route(&self) -> Option<&Route> {
        self.route.as_ref()
    }

    /// All attempts, oldest first.
    #[must_use]
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// The attempt in flight, if any.
    #[must_use]
    pub fn active_attempt(&self) -> Option<&Attempt> {
        self.attempts.iter().rev().find(|a| a.is_active())
    }

    /// The most recent attempt.
    #[must_use]
    pub fn last_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    /// Status.
    #[must_use]
    pub const fn status(&self) -> TaskStatus {
        self.status
    }

    /// Budget.
    #[must_use]
    pub const fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Cumulative usage over every attempt.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Artifacts from successful attempts.
    #[must_use]
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    /// Attempts started so far.
    #[must_use]
    pub fn attempts_used(&self) -> u8 {
        u8::try_from(self.attempts.len()).unwrap_or(u8::MAX)
    }

    /// `status == failed` and another attempt is allowed (attempts < max and
    /// the last failure class is retryable).
    #[must_use]
    pub fn can_retry(&self) -> bool {
        self.status == TaskStatus::Failed
            && self.attempts_used() < self.budget.max_attempts
            && self
                .last_attempt()
                .and_then(|a| a.failure.as_ref())
                .is_some_and(|f| f.class.is_retryable())
    }

    /// Terminal for the run: succeeded, cancelled, skipped, or failed without
    /// a possible retry.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self.status {
            TaskStatus::Succeeded | TaskStatus::Cancelled | TaskStatus::Skipped => true,
            TaskStatus::Failed => !self.can_retry(),
            _ => false,
        }
    }

    // -- helpers -----------------------------------------------------------

    fn reject(&self, cmd: &TaskCommand) -> DomainError {
        DomainError::invalid_transition(TASK_AGGREGATE_TYPE, self.status.as_str(), cmd.name())
    }

    fn require_status(&self, cmd: &TaskCommand, allowed: &[TaskStatus]) -> Result<(), DomainError> {
        if self.version == 0 {
            return Err(DomainError::NotFound {
                aggregate: TASK_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if allowed.contains(&self.status) {
            Ok(())
        } else {
            Err(self.reject(cmd))
        }
    }

    fn require_active(&self, attempt_id: AttemptId) -> Result<&Attempt, DomainError> {
        match self.active_attempt() {
            Some(a) if a.id == attempt_id => Ok(a),
            other => Err(DomainError::AttemptMismatch {
                expected: other.map(|a| a.id),
                got: attempt_id,
            }),
        }
    }

    fn handle_create(&self, cmd: &CreateTask) -> Result<Vec<TaskEvent>, DomainError> {
        if self.version > 0 {
            return Err(DomainError::AlreadyExists {
                aggregate: TASK_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if cmd.spec.title.trim().is_empty() {
            return Err(DomainError::invalid_value(
                "spec.title",
                "must not be empty",
            ));
        }
        if cmd.budget.max_attempts == 0 {
            return Err(DomainError::invalid_value(
                "budget.max_attempts",
                "must be at least 1",
            ));
        }
        Ok(vec![TaskEvent::Created {
            task_id: cmd.task_id,
            run_id: cmd.run_id,
            kind: cmd.kind.clone(),
            spec: cmd.spec.clone(),
            budget: cmd.budget.clone(),
        }])
    }

    fn handle_start_attempt(&self, cmd: &StartAttempt) -> Result<Vec<TaskEvent>, DomainError> {
        let route = self.route.clone().ok_or(DomainError::RouteRequired)?;
        if let Some(active) = self.active_attempt() {
            return Err(DomainError::AttemptAlreadyRunning {
                attempt_id: active.id,
            });
        }
        let used = self.attempts_used();
        if used >= self.budget.max_attempts {
            return Err(DomainError::AttemptsExhausted {
                attempts: used,
                max: self.budget.max_attempts,
            });
        }
        if self.attempts.iter().any(|a| a.id == cmd.attempt_id) {
            return Err(DomainError::invalid_value(
                "attempt_id",
                "attempt id already used on this task",
            ));
        }
        Ok(vec![TaskEvent::AttemptStarted {
            attempt_id: cmd.attempt_id,
            attempt_no: used + 1,
            route,
            workspace: cmd.workspace.clone(),
            worker_session_id: cmd.worker_session_id.clone(),
        }])
    }

    fn handle_fail_attempt(&self, cmd: &FailAttempt) -> Result<Vec<TaskEvent>, DomainError> {
        self.require_active(cmd.attempt_id)?;
        let retry_possible =
            cmd.class.is_retryable() && self.attempts_used() < self.budget.max_attempts;
        let mut events = vec![TaskEvent::AttemptFailed {
            attempt_id: cmd.attempt_id,
            class: cmd.class,
            message: cmd.message.clone(),
            usage: cmd.usage,
            retry_possible,
        }];
        if cmd.class == FailureClass::Cancelled {
            events.push(TaskEvent::Cancelled {
                reason: cmd.message.clone(),
            });
        }
        Ok(events)
    }

    fn handle_retry(&self, cmd: &RetryTask) -> Result<Vec<TaskEvent>, DomainError> {
        let used = self.attempts_used();
        if used >= self.budget.max_attempts {
            return Err(DomainError::AttemptsExhausted {
                attempts: used,
                max: self.budget.max_attempts,
            });
        }
        let class = self
            .last_attempt()
            .and_then(|a| a.failure.as_ref())
            .map_or(FailureClass::Permanent, |f| f.class);
        if !class.is_retryable() {
            return Err(DomainError::NotRetryable { class });
        }
        Ok(vec![TaskEvent::Retried {
            next_attempt_no: used + 1,
            reason: cmd.reason.clone(),
        }])
    }

    fn finish_active_attempt(&mut self, attempt_id: AttemptId, f: impl FnOnce(&mut Attempt)) {
        if let Some(attempt) = self.attempts.iter_mut().find(|a| a.id == attempt_id) {
            f(attempt);
        }
        self.usage = self.attempts.iter().map(|a| a.usage).sum();
    }
}

impl Aggregate for Task {
    type Command = TaskCommand;
    type Event = TaskEvent;

    const TYPE: &'static str = TASK_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        self.id.as_uuid()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn handle(&self, cmd: &TaskCommand) -> Result<Vec<TaskEvent>, DomainError> {
        use TaskStatus as S;
        match cmd {
            TaskCommand::Create(c) => self.handle_create(c),
            TaskCommand::Route(c) => {
                self.require_status(cmd, &[S::Pending, S::Routed])?;
                Ok(vec![TaskEvent::Routed {
                    route: c.route.clone(),
                    selection: c.selection.clone(),
                }])
            }
            TaskCommand::StartAttempt(c) => {
                self.require_status(cmd, &[S::Routed])?;
                self.handle_start_attempt(c)
            }
            TaskCommand::RecordProgress(c) => {
                self.require_status(cmd, &[S::Running])?;
                self.require_active(c.attempt_id)?;
                Ok(vec![TaskEvent::Progressed {
                    attempt_id: c.attempt_id,
                    summary: c.summary.clone(),
                    usage_delta: c.usage_delta,
                    log_seq: c.log_seq,
                }])
            }
            TaskCommand::RequestInput(c) => {
                self.require_status(cmd, &[S::Running])?;
                self.require_active(c.attempt_id)?;
                Ok(vec![TaskEvent::InputRequested {
                    attempt_id: c.attempt_id,
                    question_id: c.question_id,
                }])
            }
            TaskCommand::ProvideInput(c) => {
                self.require_status(cmd, &[S::AwaitingInput])?;
                let attempt = self.require_active(c.attempt_id)?;
                if attempt.pending_question != Some(c.question_id) {
                    return Err(DomainError::UnknownQuestion {
                        question_id: c.question_id,
                    });
                }
                Ok(vec![TaskEvent::InputProvided {
                    attempt_id: c.attempt_id,
                    question_id: c.question_id,
                }])
            }
            TaskCommand::SucceedAttempt(c) => {
                self.require_status(cmd, &[S::Running])?;
                self.require_active(c.attempt_id)?;
                Ok(vec![TaskEvent::AttemptSucceeded {
                    attempt_id: c.attempt_id,
                    artifacts: c.artifacts.clone(),
                    summary: c.summary.clone(),
                    usage: c.usage,
                }])
            }
            TaskCommand::FailAttempt(c) => {
                self.require_status(cmd, &[S::Running, S::AwaitingInput])?;
                self.handle_fail_attempt(c)
            }
            TaskCommand::Retry(c) => {
                self.require_status(cmd, &[S::Failed])?;
                self.handle_retry(c)
            }
            TaskCommand::Cancel(c) => {
                self.require_status(
                    cmd,
                    &[
                        S::Pending,
                        S::Routed,
                        S::Running,
                        S::AwaitingInput,
                        S::Failed,
                    ],
                )?;
                if self.status == S::Failed && !self.can_retry() {
                    return Err(self.reject(cmd));
                }
                Ok(vec![TaskEvent::Cancelled {
                    reason: c.reason.clone(),
                }])
            }
            TaskCommand::Skip(c) => {
                self.require_status(cmd, &[S::Pending])?;
                Ok(vec![TaskEvent::Skipped {
                    reason: c.reason.clone(),
                }])
            }
        }
    }

    fn apply(&mut self, event: &TaskEvent) {
        self.version += 1;
        match event {
            TaskEvent::Created {
                task_id,
                run_id,
                kind,
                spec,
                budget,
            } => {
                self.id = *task_id;
                self.run_id = *run_id;
                self.kind = Some(kind.clone());
                self.spec = Some(spec.clone());
                self.budget = budget.clone();
                self.status = TaskStatus::Pending;
            }
            TaskEvent::Routed { route, .. } => {
                self.route = Some(route.clone());
                self.status = TaskStatus::Routed;
            }
            TaskEvent::AttemptStarted {
                attempt_id,
                attempt_no,
                route,
                workspace,
                worker_session_id,
            } => {
                self.attempts.push(Attempt {
                    id: *attempt_id,
                    no: *attempt_no,
                    route: route.clone(),
                    workspace: workspace.clone(),
                    worker_session_id: worker_session_id.clone(),
                    status: AttemptStatus::Running,
                    usage: Usage::ZERO,
                    summary: None,
                    failure: None,
                    pending_question: None,
                    last_log_seq: 0,
                });
                self.status = TaskStatus::Running;
            }
            TaskEvent::Progressed {
                attempt_id,
                summary,
                usage_delta,
                log_seq,
            } => {
                self.finish_active_attempt(*attempt_id, |a| {
                    a.usage += *usage_delta;
                    a.summary = Some(summary.clone());
                    a.last_log_seq = a.last_log_seq.max(*log_seq);
                });
            }
            TaskEvent::InputRequested {
                attempt_id,
                question_id,
            } => {
                self.finish_active_attempt(*attempt_id, |a| {
                    a.status = AttemptStatus::AwaitingInput;
                    a.pending_question = Some(*question_id);
                });
                self.status = TaskStatus::AwaitingInput;
            }
            TaskEvent::InputProvided { attempt_id, .. } => {
                self.finish_active_attempt(*attempt_id, |a| {
                    a.status = AttemptStatus::Running;
                    a.pending_question = None;
                });
                self.status = TaskStatus::Running;
            }
            TaskEvent::AttemptSucceeded {
                attempt_id,
                artifacts,
                summary,
                usage,
            } => {
                self.finish_active_attempt(*attempt_id, |a| {
                    a.status = AttemptStatus::Succeeded;
                    a.summary = Some(summary.clone());
                    a.pending_question = None;
                    if !usage.is_zero() {
                        a.usage = *usage;
                    }
                });
                self.artifacts.extend(artifacts.iter().cloned());
                self.status = TaskStatus::Succeeded;
            }
            TaskEvent::AttemptFailed {
                attempt_id,
                class,
                message,
                usage,
                ..
            } => {
                self.finish_active_attempt(*attempt_id, |a| {
                    a.status = AttemptStatus::Failed;
                    a.failure = Some(AttemptFailure {
                        class: *class,
                        message: message.clone(),
                    });
                    a.pending_question = None;
                    if !usage.is_zero() {
                        a.usage = *usage;
                    }
                });
                self.status = TaskStatus::Failed;
            }
            TaskEvent::Retried { .. } => {
                self.status = TaskStatus::Routed;
            }
            TaskEvent::Cancelled { reason } => {
                if let Some(active) = self.active_attempt().map(|a| a.id) {
                    self.finish_active_attempt(active, |a| {
                        a.status = AttemptStatus::Failed;
                        a.failure = Some(AttemptFailure {
                            class: FailureClass::Cancelled,
                            message: reason.clone(),
                        });
                        a.pending_question = None;
                    });
                }
                self.status = TaskStatus::Cancelled;
            }
            TaskEvent::Skipped { .. } => {
                self.status = TaskStatus::Skipped;
            }
        }
    }
}
