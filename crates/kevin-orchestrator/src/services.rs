//! Application services (`plan/05-orchestration.md` §1).
//!
//! Each service is a thin command handler:
//!
//! ```text
//! processed_commands lookup → load stream → rehydrate → aggregate.handle(cmd)
//!   → append(expected_version) → record result → publish to the bus
//! ```
//!
//! No business rule lives here: the aggregates in `kevin-domain` decide, the
//! store enforces optimistic concurrency and the bus only learns about events
//! that are already committed.
//!
//! - **Idempotency.** Every command carries a `CommandId`. A hit in
//!   `core.processed_commands` returns the recorded result without
//!   re-executing ([`CommandOutcome::replayed`]).
//! - **OCC retry.** [`StoreError::VersionConflict`] reloads and re-applies the
//!   command at most [`OCC_RETRIES`] times with 10/50/200 ms backoff; after
//!   that the caller gets [`AppError::Conflict`] and the saga treats it as
//!   transient.
//! - **Publishing.** `append` returns the persisted envelopes; they are
//!   published after commit. A bus failure is logged, never fails the command
//!   (cross-process consumers catch up from the store).

use std::sync::Arc;
use std::time::Duration;

use kevin_bus::EventBus;
use kevin_domain::question::{AnswerQuestion, AskQuestion, ExpireQuestion, QuestionCommand};
use kevin_domain::run::{
    ApprovePlan, CancelRun, Evaluate, ExhaustBudget, FailRun, MarkEvaluated, MarkIntegrated,
    NoteQuestionAnswered, NoteTaskTerminal, ProposePlan, RecordTaskUsage, RecordUnderstanding,
    RejectPlan, RunCommand, StartExecution, StartRun, StartUnderstanding,
};
use kevin_domain::task::{
    CancelTask, CreateTask, FailAttempt, ProvideInput, RecordProgress, RequestInput, RetryTask,
    RouteTask, SkipTask, StartAttempt, SucceedAttempt, TaskCommand,
};
use kevin_domain::{
    Actor, Aggregate, Clock, CommandId, DomainError, EventMeta, IdGen, Question, QuestionId, Run,
    RunId, Task, TaskId,
};
use kevin_store::{EventStore, NewEvent, StoreError, StoredEvent, StreamId};
use kevin_telemetry::metrics as metric_names;
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

use crate::error::AppError;
use crate::ports::CommandIdempotency;

/// Number of times a command is replayed after a version conflict.
pub const OCC_RETRIES: u32 = 3;

/// Backoff before each OCC replay (`plan/05` §1).
const OCC_BACKOFF: [Duration; OCC_RETRIES as usize] = [
    Duration::from_millis(10),
    Duration::from_millis(50),
    Duration::from_millis(200),
];

/// Who issued a command and under which correlation.
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// Idempotency key of the command.
    pub command_id: CommandId,
    /// Who issued it.
    pub actor: Actor,
    /// The run every event of this command correlates to.
    pub correlation_id: RunId,
}

impl CommandContext {
    /// A context for `run_id` issued by `actor`.
    #[must_use]
    pub const fn new(command_id: CommandId, actor: Actor, correlation_id: RunId) -> Self {
        Self {
            command_id,
            actor,
            correlation_id,
        }
    }

    /// A context issued by the orchestrator itself, with a fresh command id.
    pub fn system(ids: &dyn IdGen, correlation_id: RunId) -> Self {
        Self::new(
            ids.command_id(),
            Actor::system(SYSTEM_COMPONENT),
            correlation_id,
        )
    }

    /// A context issued by a user, with a fresh command id.
    pub fn user(ids: &dyn IdGen, correlation_id: RunId, name: impl Into<String>) -> Self {
        Self::new(ids.command_id(), Actor::user(name), correlation_id)
    }
}

/// `Actor::System { component }` value used by the engine.
pub const SYSTEM_COMPONENT: &str = "orchestrator";

/// What a service call produced.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// The recorded (or freshly computed) result.
    pub result: Value,
    /// Events appended by this call; empty on a replay.
    pub events: Vec<StoredEvent>,
    /// The command id had already been processed.
    pub replayed: bool,
}

impl CommandOutcome {
    /// The `event_type`s appended by this call.
    #[must_use]
    pub fn event_types(&self) -> Vec<&'static str> {
        self.events.iter().map(|e| e.envelope.event_type).collect()
    }
}

/// Shared plumbing of the three services.
#[derive(Clone)]
pub struct ServiceCore {
    store: Arc<dyn EventStore>,
    bus: Arc<dyn EventBus>,
    commands: Arc<dyn CommandIdempotency>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl std::fmt::Debug for ServiceCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceCore").finish_non_exhaustive()
    }
}

impl ServiceCore {
    /// Wires the store, bus, command log, clock and id generator.
    #[must_use]
    pub fn new(
        store: Arc<dyn EventStore>,
        bus: Arc<dyn EventBus>,
        commands: Arc<dyn CommandIdempotency>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGen>,
    ) -> Self {
        Self {
            store,
            bus,
            commands,
            clock,
            ids,
        }
    }

    /// The event store.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn EventStore> {
        &self.store
    }

    /// The event bus.
    #[must_use]
    pub fn bus(&self) -> &Arc<dyn EventBus> {
        &self.bus
    }

    /// The clock.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The id generator.
    #[must_use]
    pub fn ids(&self) -> &Arc<dyn IdGen> {
        &self.ids
    }

    /// Rehydrates an aggregate from its stream.
    pub async fn load<A>(&self, id: Uuid) -> Result<A, AppError>
    where
        A: Aggregate,
        A::Event: DeserializeOwned,
    {
        let stream = StreamId::new(A::TYPE, id);
        let stored = self.store.load_stream(&stream, 0).await?;
        let mut aggregate = A::default();
        for event in stored {
            let decoded: A::Event =
                serde_json::from_value(event.envelope.payload).map_err(|e| AppError::Corrupt {
                    stream: stream.to_string(),
                    message: e.to_string(),
                })?;
            aggregate.apply(&decoded);
        }
        Ok(aggregate)
    }

    /// Runs one command against one aggregate with idempotency + OCC retry.
    pub async fn execute<A>(
        &self,
        id: Uuid,
        cmd: &A::Command,
        ctx: &CommandContext,
        result: Value,
    ) -> Result<CommandOutcome, AppError>
    where
        A: Aggregate,
        A::Event: DeserializeOwned,
    {
        if let Some(previous) = self.commands.begin(ctx.command_id).await? {
            tracing::debug!(command_id = %ctx.command_id, aggregate = A::TYPE, "command replayed");
            return Ok(CommandOutcome {
                result: previous,
                events: Vec::new(),
                replayed: true,
            });
        }
        let stream = StreamId::new(A::TYPE, id);
        let mut retries = 0;
        loop {
            let aggregate: A = self.load(id).await?;
            let expected = aggregate.version();
            let events = aggregate
                .handle(cmd)
                .map_err(map_domain_error(A::TYPE, id))?;
            let new_events: Vec<NewEvent> = events
                .iter()
                .map(|event| self.new_event(event, ctx))
                .collect::<Result<_, _>>()?;
            let started = std::time::Instant::now();
            match self.store.append(&stream, expected, &new_events).await {
                Ok(appended) => {
                    metrics::histogram!(
                        metric_names::EVENT_STORE_APPEND_DURATION_SECONDS,
                        "aggregate_type" => A::TYPE,
                    )
                    .record(started.elapsed().as_secs_f64());
                    for event in &appended.events {
                        metrics::counter!(
                            metric_names::EVENTS_APPENDED_TOTAL,
                            "event_type" => event.envelope.event_type,
                        )
                        .increment(1);
                    }
                    let recorded = self.commands.complete(ctx.command_id, &result).await?;
                    self.publish(&appended.events).await;
                    return Ok(CommandOutcome {
                        result: recorded,
                        events: appended.events,
                        replayed: false,
                    });
                }
                Err(StoreError::VersionConflict { .. }) if retries < OCC_RETRIES => {
                    metrics::counter!(
                        metric_names::EVENT_STORE_VERSION_CONFLICTS_TOTAL,
                        "aggregate_type" => A::TYPE,
                    )
                    .increment(1);
                    tokio::time::sleep(OCC_BACKOFF[retries as usize]).await;
                    retries += 1;
                }
                Err(StoreError::VersionConflict { .. }) => {
                    metrics::counter!(
                        metric_names::EVENT_STORE_VERSION_CONFLICTS_TOTAL,
                        "aggregate_type" => A::TYPE,
                    )
                    .increment(1);
                    return Err(AppError::Conflict {
                        stream: stream.to_string(),
                        attempts: retries + 1,
                    });
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn new_event<E: EventMeta>(
        &self,
        event: &E,
        ctx: &CommandContext,
    ) -> Result<NewEvent, AppError> {
        Ok(NewEvent {
            event_id: self.ids.event_id(),
            event_type: event.event_type(),
            schema_version: event.schema_version(),
            occurred_at: self.clock.now(),
            correlation_id: ctx.correlation_id.as_uuid(),
            causation_id: Some(ctx.command_id.as_uuid()),
            actor: ctx.actor.clone(),
            payload: serde_json::to_value(event).map_err(StoreError::from)?,
        })
    }

    async fn publish(&self, events: &[StoredEvent]) {
        let envelopes: Vec<kevin_bus::Event> = events.iter().map(|e| e.envelope.clone()).collect();
        if let Err(err) = self.bus.publish(&envelopes).await {
            tracing::warn!(error = %err, "bus publish failed; consumers catch up from the store");
        }
    }
}

fn map_domain_error(aggregate: &'static str, id: Uuid) -> impl Fn(DomainError) -> AppError + use<> {
    move |err| match err {
        DomainError::NotFound { .. } => AppError::NotFound { aggregate, id },
        other => AppError::Domain(other),
    }
}

fn unit() -> Value {
    Value::Null
}

// ---------------------------------------------------------------------------
// RunService
// ---------------------------------------------------------------------------

/// Commands of the [`Run`] aggregate (`plan/05` §1).
#[derive(Debug, Clone)]
pub struct RunService {
    core: ServiceCore,
}

impl RunService {
    /// Wraps the shared plumbing.
    #[must_use]
    pub const fn new(core: ServiceCore) -> Self {
        Self { core }
    }

    /// The shared plumbing (store, bus, clock, ids).
    #[must_use]
    pub const fn core(&self) -> &ServiceCore {
        &self.core
    }

    /// Loads the run aggregate.
    pub async fn load(&self, run_id: RunId) -> Result<Run, AppError> {
        self.core.load::<Run>(run_id.as_uuid()).await
    }

    /// Runs any [`RunCommand`].
    pub async fn dispatch(
        &self,
        run_id: RunId,
        cmd: RunCommand,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.core
            .execute::<Run>(run_id.as_uuid(), &cmd, ctx, unit())
            .await
    }

    /// Creates the run (`run.started`).
    pub async fn start(&self, cmd: StartRun, ctx: &CommandContext) -> Result<RunId, AppError> {
        let run_id = cmd.run_id;
        let result = Value::String(run_id.to_string());
        let outcome = self
            .core
            .execute::<Run>(run_id.as_uuid(), &RunCommand::Start(cmd), ctx, result)
            .await?;
        outcome
            .result
            .as_str()
            .and_then(|s| s.parse::<RunId>().ok())
            .ok_or(AppError::Duplicate(outcome.result))
    }

    /// Records that the planner call started (`run.understanding_started`).
    pub async fn start_understanding(
        &self,
        run_id: RunId,
        cmd: StartUnderstanding,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::StartUnderstanding(cmd), ctx)
            .await
    }

    /// Records the planner's understanding (`run.understanding_completed`).
    pub async fn record_understanding(
        &self,
        run_id: RunId,
        cmd: RecordUnderstanding,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::RecordUnderstanding(cmd), ctx)
            .await
    }

    /// Notes that one clarification question was answered.
    pub async fn note_question_answered(
        &self,
        run_id: RunId,
        cmd: NoteQuestionAnswered,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::NoteQuestionAnswered(cmd), ctx)
            .await
    }

    /// Records a plan proposal (`run.plan_proposed`).
    pub async fn propose_plan(
        &self,
        run_id: RunId,
        cmd: ProposePlan,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::ProposePlan(cmd), ctx)
            .await
    }

    /// Approves the proposed plan (`run.plan_approved`).
    pub async fn approve_plan(
        &self,
        run_id: RunId,
        cmd: ApprovePlan,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::ApprovePlan(cmd), ctx)
            .await
    }

    /// Rejects the proposed plan (`run.plan_rejected`), triggering a re-plan.
    pub async fn reject_plan(
        &self,
        run_id: RunId,
        cmd: RejectPlan,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::RejectPlan(cmd), ctx)
            .await
    }

    /// Records the tasks created for the approved plan (`run.execution_started`).
    pub async fn start_execution(
        &self,
        run_id: RunId,
        cmd: StartExecution,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::StartExecution(cmd), ctx)
            .await
    }

    /// Rolls a task's cumulative usage up onto the run.
    pub async fn record_task_usage(
        &self,
        run_id: RunId,
        cmd: RecordTaskUsage,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::RecordTaskUsage(cmd), ctx)
            .await
    }

    /// Notes a terminal task (`run.task_terminal_noted`).
    pub async fn note_task_terminal(
        &self,
        run_id: RunId,
        cmd: NoteTaskTerminal,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::NoteTaskTerminal(cmd), ctx)
            .await
    }

    /// Reports a budget dimension the run cannot observe itself (wall-clock).
    pub async fn exhaust_budget(
        &self,
        run_id: RunId,
        cmd: ExhaustBudget,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::ExhaustBudget(cmd), ctx)
            .await
    }

    /// Records the integration result (`run.integrated`).
    pub async fn mark_integrated(
        &self,
        run_id: RunId,
        cmd: MarkIntegrated,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::MarkIntegrated(cmd), ctx)
            .await
    }

    /// Records the evaluation and completes the run.
    pub async fn mark_evaluated(
        &self,
        run_id: RunId,
        cmd: MarkEvaluated,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::MarkEvaluated(cmd), ctx)
            .await
    }

    /// Cancels the run (`run.cancelled`).
    pub async fn cancel(
        &self,
        run_id: RunId,
        cmd: CancelRun,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::Cancel(cmd), ctx).await
    }

    /// Fails the run (`run.failed`).
    pub async fn fail(
        &self,
        run_id: RunId,
        cmd: FailRun,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::Fail(cmd), ctx).await
    }

    /// Requests a re-evaluation of a terminal run.
    pub async fn evaluate(
        &self,
        run_id: RunId,
        cmd: Evaluate,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(run_id, RunCommand::Evaluate(cmd), ctx).await
    }
}

// ---------------------------------------------------------------------------
// TaskService
// ---------------------------------------------------------------------------

/// Commands of the [`Task`] aggregate (`plan/05` §1).
#[derive(Debug, Clone)]
pub struct TaskService {
    core: ServiceCore,
}

impl TaskService {
    /// Wraps the shared plumbing.
    #[must_use]
    pub const fn new(core: ServiceCore) -> Self {
        Self { core }
    }

    /// The shared plumbing.
    #[must_use]
    pub const fn core(&self) -> &ServiceCore {
        &self.core
    }

    /// Loads the task aggregate.
    pub async fn load(&self, task_id: TaskId) -> Result<Task, AppError> {
        self.core.load::<Task>(task_id.as_uuid()).await
    }

    /// Runs any [`TaskCommand`].
    pub async fn dispatch(
        &self,
        task_id: TaskId,
        cmd: TaskCommand,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.core
            .execute::<Task>(task_id.as_uuid(), &cmd, ctx, unit())
            .await
    }

    /// Creates the task (`task.created`).
    pub async fn create_task(
        &self,
        cmd: CreateTask,
        ctx: &CommandContext,
    ) -> Result<TaskId, AppError> {
        let task_id = cmd.task_id;
        let result = Value::String(task_id.to_string());
        let outcome = self
            .core
            .execute::<Task>(task_id.as_uuid(), &TaskCommand::Create(cmd), ctx, result)
            .await?;
        outcome
            .result
            .as_str()
            .and_then(|s| s.parse::<TaskId>().ok())
            .ok_or(AppError::Duplicate(outcome.result))
    }

    /// Assigns a route (`task.routed`).
    pub async fn route_task(
        &self,
        task_id: TaskId,
        cmd: RouteTask,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::Route(cmd), ctx).await
    }

    /// Starts an attempt (`task.attempt_started`).
    pub async fn start_attempt(
        &self,
        task_id: TaskId,
        cmd: StartAttempt,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::StartAttempt(cmd), ctx)
            .await
    }

    /// Records throttled progress (`task.progressed`).
    pub async fn record_progress(
        &self,
        task_id: TaskId,
        cmd: RecordProgress,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::RecordProgress(cmd), ctx)
            .await
    }

    /// Records that the worker asked a question (`task.input_requested`).
    pub async fn request_input(
        &self,
        task_id: TaskId,
        cmd: RequestInput,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::RequestInput(cmd), ctx)
            .await
    }

    /// Records that the answer reached the worker (`task.input_provided`).
    pub async fn provide_input(
        &self,
        task_id: TaskId,
        cmd: ProvideInput,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::ProvideInput(cmd), ctx)
            .await
    }

    /// Finishes an attempt successfully (`task.attempt_succeeded`).
    pub async fn succeed_attempt(
        &self,
        task_id: TaskId,
        cmd: SucceedAttempt,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::SucceedAttempt(cmd), ctx)
            .await
    }

    /// Fails an attempt (`task.attempt_failed`).
    pub async fn fail_attempt(
        &self,
        task_id: TaskId,
        cmd: FailAttempt,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::FailAttempt(cmd), ctx)
            .await
    }

    /// Allows another attempt (`task.retried`).
    pub async fn retry_task(
        &self,
        task_id: TaskId,
        cmd: RetryTask,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::Retry(cmd), ctx).await
    }

    /// Cancels the task (`task.cancelled`).
    pub async fn cancel_task(
        &self,
        task_id: TaskId,
        cmd: CancelTask,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::Cancel(cmd), ctx).await
    }

    /// Skips a pending task (`task.skipped`).
    pub async fn skip_task(
        &self,
        task_id: TaskId,
        cmd: SkipTask,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.dispatch(task_id, TaskCommand::Skip(cmd), ctx).await
    }
}

// ---------------------------------------------------------------------------
// QuestionService
// ---------------------------------------------------------------------------

/// Commands of the [`Question`] aggregate (`plan/05` §1, §3.3).
#[derive(Debug, Clone)]
pub struct QuestionService {
    core: ServiceCore,
}

impl QuestionService {
    /// Wraps the shared plumbing.
    #[must_use]
    pub const fn new(core: ServiceCore) -> Self {
        Self { core }
    }

    /// The shared plumbing.
    #[must_use]
    pub const fn core(&self) -> &ServiceCore {
        &self.core
    }

    /// Loads the question aggregate.
    pub async fn load(&self, question_id: QuestionId) -> Result<Question, AppError> {
        self.core.load::<Question>(question_id.as_uuid()).await
    }

    /// Asks a question (`question.asked`).
    pub async fn ask(
        &self,
        cmd: AskQuestion,
        ctx: &CommandContext,
    ) -> Result<QuestionId, AppError> {
        let question_id = cmd.question_id;
        let result = Value::String(question_id.to_string());
        let outcome = self
            .core
            .execute::<Question>(
                question_id.as_uuid(),
                &QuestionCommand::Ask(cmd),
                ctx,
                result,
            )
            .await?;
        outcome
            .result
            .as_str()
            .and_then(|s| s.parse::<QuestionId>().ok())
            .ok_or(AppError::Duplicate(outcome.result))
    }

    /// Answers an open question (`question.answered`).
    pub async fn answer(
        &self,
        question_id: QuestionId,
        cmd: AnswerQuestion,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.core
            .execute::<Question>(
                question_id.as_uuid(),
                &QuestionCommand::Answer(cmd),
                ctx,
                unit(),
            )
            .await
    }

    /// Expires an open question (`question.expired`, plus `question.answered`
    /// when a default exists).
    pub async fn expire(
        &self,
        question_id: QuestionId,
        ctx: &CommandContext,
    ) -> Result<CommandOutcome, AppError> {
        self.core
            .execute::<Question>(
                question_id.as_uuid(),
                &QuestionCommand::Expire(ExpireQuestion),
                ctx,
                unit(),
            )
            .await
    }
}
