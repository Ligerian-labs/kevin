//! The [`TaskRunner`] (`plan/05-orchestration.md` §3.5).
//!
//! One tokio task per attempt. It owns the worker handle and folds its
//! [`WorkerEvent`] stream into domain commands:
//!
//! ```text
//! Starting ──start() ok──▶ Streaming ──Final──▶ Validating ──▶ Succeeded
//!    │                        │  ▲                  │
//!    │                        │  └── answer ────────┼── AwaitingInput
//!    └── spawn error ─────────┴──────────────────────┴──▶ Failed
//! ```
//!
//! Folding rules:
//!
//! - every event advances a monotone `log_seq` (the `orch.task_log` sequence
//!   WS-11 persists) and `task.progressed` is emitted at most once per
//!   `orchestrator.progress_interval`, or on a milestone (every 25 tool calls,
//!   or more than 50 000 tokens since the last one);
//! - `Usage` deltas are accumulated, priced through the model catalog when the
//!   worker reports no cost, rolled up onto the run (`RecordTaskUsage`, which
//!   is what makes the run emit `run.budget_exhausted`) and checked against the
//!   task budget;
//! - `InputRequested` becomes a `Question` plus `task.input_requested`; the
//!   answer arrives from the actor as [`RunnerInput::Answered`] and produces
//!   `task.input_provided`;
//! - the terminal event, cancellation, the attempt timeout and the task budget
//!   all converge on exactly one `task.attempt_succeeded` / `task.attempt_failed`.
//!
//! The runner never decides *whether* to retry — that is the saga's job
//! ([`crate::run_actor`]); it only classifies the failure.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kevin_domain::question::AskQuestion;
use kevin_domain::run::RecordTaskUsage;
use kevin_domain::task::{FailAttempt, ProvideInput, RecordProgress, RequestInput, SucceedAttempt};
use kevin_domain::{
    Answer, ArtifactRef, AttemptId, Budget, FailureClass, QuestionId, QuestionOption,
    QuestionPolicy, Route, RunId, RunMode, TaskId, TaskKind, TaskSpec, Usage, Workspace,
};
use kevin_telemetry::metrics as metric_names;
use kevin_worker::types::{AttemptBudget, AttemptContext, TaskAttemptRequest};
use kevin_worker::usage::finalize_cost;
use kevin_worker::{WorkerEvent, WorkerSessionId, structured};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::convert;
use crate::orchestrator::OrchestratorDeps;
use crate::projections::NewTaskLogLine;
use crate::scheduler::Permits;
use crate::services::CommandContext;

/// Worker log lines buffered before they are flushed to `orch.task_log`.
const LOG_FLUSH_AT: usize = 32;
/// Progress milestone: emit `task.progressed` every N tool calls.
const TOOL_CALL_MILESTONE: u32 = 25;
/// Progress milestone: emit `task.progressed` after this many new tokens.
const TOKEN_MILESTONE: u64 = 50_000;
/// `task.attempt_failed.message` when the runtime shut down under the attempt.
pub const RUNTIME_SHUTDOWN: &str = "runtime_shutdown";
/// `task.attempt_failed.message` when the attempt outlived its wall clock.
pub const ATTEMPT_TIMEOUT: &str = "task_attempt_timeout";

/// Everything the runner needs to start one attempt.
#[derive(Debug, Clone)]
pub struct AttemptSpec {
    /// The run.
    pub run_id: RunId,
    /// The task.
    pub task_id: TaskId,
    /// The attempt (fresh for every retry).
    pub attempt_id: AttemptId,
    /// 1-based attempt number (`orch.task_log.attempt`).
    pub attempt_no: i32,
    /// Task kind.
    pub kind: TaskKind,
    /// The task spec.
    pub spec: TaskSpec,
    /// Route chosen by the router (or `[roles]`).
    pub route: Route,
    /// Workspace prepared for this attempt.
    pub workspace: Workspace,
    /// Effective task budget (`budget.default_task_*`).
    pub budget: Budget,
    /// Wall clock of the attempt.
    pub timeout: Duration,
    /// Interaction mode (drives the question policy).
    pub mode: RunMode,
    /// Rendered `<kevin-memory>` block.
    pub memory: Option<String>,
    /// Kevin briefing appended to the worker's system prompt.
    pub system_prompt_append: String,
    /// Worker-native session to resume (retry after an unanswered question).
    pub prior_session: Option<WorkerSessionId>,
}

/// Control messages the actor sends to a running attempt.
#[derive(Debug, Clone)]
pub enum RunnerInput {
    /// A question the attempt asked was answered.
    Answered {
        /// The question.
        question_id: QuestionId,
        /// The answer.
        answer: Answer,
    },
    /// Terminate the attempt with this classification (drain/shutdown, budget).
    Stop {
        /// Class recorded on `task.attempt_failed`.
        class: FailureClass,
        /// Message recorded on `task.attempt_failed`.
        message: String,
    },
}

/// How one attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// `task.attempt_succeeded` was recorded.
    Succeeded,
    /// `task.attempt_failed` was recorded.
    Failed {
        /// Classification driving the retry policy.
        class: FailureClass,
        /// Diagnostic.
        message: String,
    },
}

impl AttemptResult {
    /// `true` for [`AttemptResult::Succeeded`].
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, AttemptResult::Succeeded)
    }

    /// The failure class, when it failed.
    #[must_use]
    pub const fn failure_class(&self) -> Option<FailureClass> {
        match self {
            AttemptResult::Succeeded => None,
            AttemptResult::Failed { class, .. } => Some(*class),
        }
    }
}

/// What the actor learns when a runner finishes.
#[derive(Debug, Clone)]
pub struct TaskRunnerOutcome {
    /// The run.
    pub run_id: RunId,
    /// The task.
    pub task_id: TaskId,
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Task kind (routing outcome).
    pub kind: TaskKind,
    /// Route used (routing outcome).
    pub route: Route,
    /// Workspace of the attempt (integration input).
    pub workspace: Workspace,
    /// How it ended.
    pub result: AttemptResult,
    /// Final usage of the attempt.
    pub usage: Usage,
    /// Wall-clock of the attempt.
    pub wall_ms: u64,
}

/// Phase of the attempt state machine (tracing/metrics label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Spawning the worker.
    Starting,
    /// Consuming the worker stream.
    Streaming,
    /// Paused on a question.
    AwaitingInput,
    /// Checking the structured output.
    Validating,
    /// Terminal: success.
    Succeeded,
    /// Terminal: failure.
    Failed,
}

impl Phase {
    /// `snake_case` name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Streaming => "streaming",
            Phase::AwaitingInput => "awaiting_input",
            Phase::Validating => "validating",
            Phase::Succeeded => "succeeded",
            Phase::Failed => "failed",
        }
    }
}

/// Drives one task attempt end to end.
pub struct TaskRunner {
    deps: Arc<OrchestratorDeps>,
    attempt: AttemptSpec,
    cancel: CancellationToken,
    inbox: mpsc::Receiver<RunnerInput>,
    permits: Option<Permits>,
    phase: Phase,
    usage: Usage,
    log_seq: u64,
    tool_calls: u32,
    tokens_since_progress: u64,
    last_progress: Instant,
    last_summary: String,
    pending_question: Option<QuestionId>,
    session_id: Option<WorkerSessionId>,
    stop: Option<(FailureClass, String)>,
    terminal: Option<WorkerEvent>,
    artifacts: Vec<ArtifactRef>,
    summary: String,
    log_buffer: Vec<NewTaskLogLine>,
}

impl std::fmt::Debug for TaskRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskRunner")
            .field("task_id", &self.attempt.task_id)
            .field("attempt_id", &self.attempt.attempt_id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl TaskRunner {
    /// Builds a runner for `attempt`; `cancel` is the attempt-level child of
    /// the run token and `permits` are released when the runner drops.
    #[must_use]
    pub fn new(
        deps: Arc<OrchestratorDeps>,
        attempt: AttemptSpec,
        cancel: CancellationToken,
        inbox: mpsc::Receiver<RunnerInput>,
        permits: Option<Permits>,
    ) -> Self {
        Self {
            deps,
            attempt,
            cancel,
            inbox,
            permits,
            phase: Phase::Starting,
            usage: Usage::ZERO,
            log_seq: 0,
            tool_calls: 0,
            tokens_since_progress: 0,
            last_progress: Instant::now(),
            last_summary: String::new(),
            pending_question: None,
            session_id: None,
            stop: None,
            terminal: None,
            artifacts: Vec::new(),
            summary: String::new(),
            log_buffer: Vec::new(),
        }
    }

    /// Runs the attempt and records exactly one terminal task event.
    #[allow(clippy::too_many_lines)]
    pub async fn run(mut self) -> TaskRunnerOutcome {
        let started = Instant::now();
        let span = tracing::info_span!(
            "attempt",
            run_id = %self.attempt.run_id,
            task_id = %self.attempt.task_id,
            attempt_id = %self.attempt.attempt_id,
            worker = self.attempt.route.worker.as_str(),
            model_alias = self.attempt.route.model.as_str(),
        );
        let _guard = span.enter();

        let result = match self.start_worker().await {
            Ok(handle) => self.stream(handle).await,
            Err(result) => result,
        };
        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.usage.wall_ms = self.usage.wall_ms.max(wall_ms);
        self.finalize_cost();

        self.flush_log().await;
        let result = self.record_terminal(result).await;
        self.phase = if result.is_success() {
            Phase::Succeeded
        } else {
            Phase::Failed
        };
        self.record_metrics(&result, started.elapsed());
        self.cleanup_workspace(result.is_success()).await;
        drop(self.permits.take());

        TaskRunnerOutcome {
            run_id: self.attempt.run_id,
            task_id: self.attempt.task_id,
            attempt_id: self.attempt.attempt_id,
            kind: self.attempt.kind.clone(),
            route: self.attempt.route.clone(),
            workspace: self.attempt.workspace.clone(),
            result,
            usage: self.usage,
            wall_ms,
        }
    }

    // -- worker ------------------------------------------------------------

    async fn start_worker(&mut self) -> Result<kevin_worker::WorkerHandle, AttemptResult> {
        let Some(worker) = self.deps.workers.get(self.attempt.route.worker) else {
            return Err(AttemptResult::Failed {
                class: FailureClass::Permanent,
                message: format!(
                    "no worker adapter registered for `{}`",
                    self.attempt.route.worker
                ),
            });
        };
        let Some(model) = self
            .deps
            .config
            .models
            .get(&self.attempt.route.model)
            .cloned()
        else {
            return Err(AttemptResult::Failed {
                class: FailureClass::Permanent,
                message: format!("unknown model alias `{}`", self.attempt.route.model),
            });
        };
        let request = TaskAttemptRequest {
            attempt_id: self.attempt.attempt_id,
            task_id: self.attempt.task_id,
            run_id: self.attempt.run_id,
            kind: self.attempt.kind.clone(),
            spec: convert::spec_to_worker(&self.attempt.spec),
            route: convert::route_to_worker(&self.attempt.route),
            model,
            workspace: convert::workspace_to_worker(&self.attempt.workspace),
            context: AttemptContext {
                system_prompt_append: self.attempt.system_prompt_append.clone(),
                memory: self.attempt.memory.clone(),
                prior_session: self.attempt.prior_session.clone(),
            },
            env: self
                .deps
                .workers
                .config()
                .env_allowlist(self.attempt.route.worker),
            budget: AttemptBudget {
                timeout: self.attempt.timeout,
                max_tokens: self.attempt.budget.max_tokens,
                max_turns: None,
            },
            cancel: self.cancel.child_token(),
        };
        match worker.start(request).await {
            Ok(handle) => {
                self.phase = Phase::Streaming;
                Ok(handle)
            }
            // A missing binary or a rejected flag is not worth a retry: the
            // adapter classifies its own errors (`plan/09` §Sandbox tiers,
            // `plan/05` §Retries).
            Err(err) => Err(AttemptResult::Failed {
                class: err.failure_class(),
                message: format!("worker spawn failed: {err}"),
            }),
        }
    }

    async fn stream(&mut self, mut handle: kevin_worker::WorkerHandle) -> AttemptResult {
        let worker_cancel = handle.cancel_token().clone();
        let deadline = tokio::time::Instant::now() + self.attempt.timeout;
        let mut cancelled = false;
        let mut timed_out = false;
        let mut inbox_open = true;
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    worker_cancel.cancel();
                }
                input = self.inbox.recv(), if inbox_open => {
                    match input {
                        Some(RunnerInput::Answered { question_id, answer }) => {
                            self.on_answer(question_id, &answer).await;
                        }
                        Some(RunnerInput::Stop { class, message }) => {
                            self.stop = Some((class, message));
                            worker_cancel.cancel();
                        }
                        None => inbox_open = false,
                    }
                }
                () = tokio::time::sleep_until(deadline), if !timed_out => {
                    timed_out = true;
                    worker_cancel.cancel();
                }
                event = handle.events.recv() => {
                    match event {
                        Some(event) => {
                            if event.is_terminal() {
                                self.terminal = Some(event);
                            } else {
                                self.fold(event).await;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        let outcome = tokio::time::timeout(
            self.deps.workers.config().kill_grace + Duration::from_secs(5),
            handle.wait(),
        )
        .await;
        let transcript = outcome
            .ok()
            .and_then(|o| o.transcript().map(convert::artifact_from_worker));

        if !cancelled && !timed_out && self.stop.is_none() {
            self.resolve_pending_input(deadline).await;
        }

        if let Some((class, message)) = self.stop.take() {
            return AttemptResult::Failed { class, message };
        }
        if cancelled {
            return AttemptResult::Failed {
                class: FailureClass::Cancelled,
                message: "run cancelled".to_owned(),
            };
        }
        if timed_out {
            return AttemptResult::Failed {
                class: FailureClass::Transient,
                message: ATTEMPT_TIMEOUT.to_owned(),
            };
        }
        match self.terminal.take() {
            Some(WorkerEvent::Final {
                text,
                structured,
                usage,
            }) => {
                let total = convert::usage_from_worker(&usage);
                if !total.is_zero() {
                    self.usage = total;
                }
                self.validate(&text, structured, transcript)
            }
            Some(WorkerEvent::Failed {
                class,
                message,
                usage,
            }) => {
                let total = convert::usage_from_worker(&usage);
                if !total.is_zero() {
                    self.usage = total;
                }
                AttemptResult::Failed { class, message }
            }
            _ => AttemptResult::Failed {
                class: FailureClass::Transient,
                message: "worker stream ended without a terminal event".to_owned(),
            },
        }
    }

    /// The worker finished while a question was still open: the attempt is
    /// only complete once `task.input_provided` is recorded, so wait for the
    /// answer until the attempt's own deadline (`plan/05` §3.5).
    async fn resolve_pending_input(&mut self, deadline: tokio::time::Instant) {
        while self.pending_question.is_some() {
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Ok(Some(RunnerInput::Answered {
                    question_id,
                    answer,
                })) => self.on_answer(question_id, &answer).await,
                Ok(Some(RunnerInput::Stop { class, message })) => {
                    self.stop = Some((class, message));
                    return;
                }
                Ok(None) | Err(_) => break,
            }
        }
        if self.pending_question.is_some() {
            self.stop = Some((FailureClass::Transient, "unanswered_input".to_owned()));
        }
    }

    // -- folding -----------------------------------------------------------

    async fn fold(&mut self, event: WorkerEvent) {
        self.log_seq += 1;
        self.log(&event);
        match event {
            WorkerEvent::Started { session_id, .. } => {
                self.session_id = session_id;
            }
            WorkerEvent::AssistantText { delta } | WorkerEvent::Thinking { delta } => {
                if !delta.trim().is_empty() {
                    self.last_summary = summarise(&delta);
                }
                self.maybe_progress(false).await;
            }
            WorkerEvent::ToolCall {
                name,
                input_summary,
            } => {
                self.tool_calls += 1;
                self.last_summary = summarise(&format!("{name}: {input_summary}"));
                let milestone = self.tool_calls.is_multiple_of(TOOL_CALL_MILESTONE);
                self.maybe_progress(milestone).await;
            }
            WorkerEvent::ToolResult { name, ok, .. } => {
                self.last_summary =
                    summarise(&format!("{name} → {}", if ok { "ok" } else { "err" }));
                self.maybe_progress(false).await;
            }
            WorkerEvent::Usage { delta } => {
                let delta = convert::usage_from_worker(&delta);
                self.usage += delta;
                self.tokens_since_progress = self
                    .tokens_since_progress
                    .saturating_add(delta.total_tokens());
                self.on_usage().await;
            }
            WorkerEvent::InputRequested { question, options } => {
                self.on_input_requested(question, options).await;
            }
            WorkerEvent::Final { .. } | WorkerEvent::Failed { .. } => {
                unreachable!("terminal events are captured by the stream loop")
            }
        }
    }

    /// Buffers one worker event as an `orch.task_log` line (WS-11).
    fn log(&mut self, event: &WorkerEvent) {
        if self.deps.task_log.is_none() {
            return;
        }
        let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
        self.log_buffer.push(
            NewTaskLogLine::new(
                self.attempt.task_id,
                self.attempt.attempt_no,
                event.kind_name(),
                payload,
            )
            .at(self.deps.clock.now())
            .run(self.attempt.run_id)
            .attempt_id(self.attempt.attempt_id),
        );
    }

    /// Writes the buffered log lines; `log_seq` follows `orch.task_log.seq`.
    async fn flush_log(&mut self) {
        let Some(task_log) = self.deps.task_log.clone() else {
            self.log_buffer.clear();
            return;
        };
        if self.log_buffer.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.log_buffer);
        match task_log.append_all(&lines).await {
            Ok(seqs) => {
                if let Some(last) = seqs.last() {
                    self.log_seq = self.log_seq.max(*last);
                }
            }
            Err(err) => tracing::warn!(error = %err, "appending task log lines failed"),
        }
    }

    async fn on_usage(&mut self) {
        self.finalize_cost();
        if let Some(excess) = self.attempt.budget.exceeded_by(&self.usage) {
            self.stop = Some((
                FailureClass::Budget,
                format!(
                    "task budget exhausted: {} limit {} exceeded by {}",
                    excess.dimension, excess.limit, excess.actual
                ),
            ));
            self.cancel.cancel();
            return;
        }
        let ctx = self.ctx();
        match self
            .deps
            .runs
            .record_task_usage(
                self.attempt.run_id,
                RecordTaskUsage {
                    task_id: self.attempt.task_id,
                    usage: self.usage,
                },
                &ctx,
            )
            .await
        {
            Ok(outcome) => {
                if outcome
                    .events
                    .iter()
                    .any(|e| e.envelope.event_type == "run.budget_exhausted")
                {
                    self.stop = Some((FailureClass::Budget, "run budget exhausted".to_owned()));
                    self.cancel.cancel();
                }
            }
            Err(err) if err.is_invalid_transition() => {}
            Err(err) => tracing::warn!(error = %err, "recording task usage failed"),
        }
        let milestone = self.tokens_since_progress > TOKEN_MILESTONE;
        self.maybe_progress(milestone).await;
    }

    async fn maybe_progress(&mut self, milestone: bool) {
        if self.phase != Phase::Streaming || self.last_summary.is_empty() {
            return;
        }
        let due = self.last_progress.elapsed() >= self.deps.config.orchestrator.progress_interval;
        if !due && !milestone {
            if self.log_buffer.len() >= LOG_FLUSH_AT {
                self.flush_log().await;
            }
            return;
        }
        self.flush_log().await;
        let ctx = self.ctx();
        let cmd = RecordProgress {
            attempt_id: self.attempt.attempt_id,
            summary: self.last_summary.clone(),
            usage_delta: Usage::ZERO,
            log_seq: self.log_seq,
        };
        match self
            .deps
            .tasks
            .record_progress(self.attempt.task_id, cmd, &ctx)
            .await
        {
            Ok(_) => {
                self.last_progress = Instant::now();
                self.tokens_since_progress = 0;
            }
            Err(err) if err.is_invalid_transition() => {}
            Err(err) => tracing::warn!(error = %err, "recording progress failed"),
        }
    }

    async fn on_input_requested(&mut self, question: String, options: Vec<String>) {
        if self.pending_question.is_some() {
            tracing::warn!("worker asked a second question while one is open; ignored");
            return;
        }
        let question_id = self.deps.ids.question_id();
        let opts: Vec<QuestionOption> = options
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let option = QuestionOption::new(label.clone());
                if i == 0 { option.recommended() } else { option }
            })
            .collect();
        let interactive = self.attempt.mode.is_interactive();
        let default = (!interactive)
            .then(|| {
                opts.first()
                    .map(|o| Answer::selected([o.label.clone()], Answer::DEFAULT_ANSWERED_BY))
            })
            .flatten();
        let policy = if interactive {
            QuestionPolicy::Block
        } else if self.attempt.mode.is_kohral() {
            QuestionPolicy::IMMEDIATE_DEFAULT
        } else {
            QuestionPolicy::DefaultAfter {
                timeout: self.deps.config.orchestrator.question_default_timeout,
            }
        };
        let ctx = self.ctx();
        let ask = AskQuestion {
            question_id,
            run_id: self.attempt.run_id,
            task_id: Some(self.attempt.task_id),
            text: question,
            options: opts,
            multi_select: false,
            default,
            policy,
        };
        if let Err(err) = self.deps.questions.ask(ask, &ctx).await {
            tracing::warn!(error = %err, "asking the worker's question failed");
            return;
        }
        let ctx = self.ctx();
        match self
            .deps
            .tasks
            .request_input(
                self.attempt.task_id,
                RequestInput {
                    attempt_id: self.attempt.attempt_id,
                    question_id,
                },
                &ctx,
            )
            .await
        {
            Ok(_) => {
                self.pending_question = Some(question_id);
                self.phase = Phase::AwaitingInput;
            }
            Err(err) => tracing::warn!(error = %err, "recording task.input_requested failed"),
        }
    }

    async fn on_answer(&mut self, question_id: QuestionId, answer: &Answer) {
        if self.pending_question != Some(question_id) {
            return;
        }
        let ctx = self.ctx();
        match self
            .deps
            .tasks
            .provide_input(
                self.attempt.task_id,
                ProvideInput {
                    attempt_id: self.attempt.attempt_id,
                    question_id,
                },
                &ctx,
            )
            .await
        {
            Ok(_) => {
                self.pending_question = None;
                self.phase = Phase::Streaming;
                self.last_summary = summarise(&answer_text(answer));
            }
            Err(err) if err.is_invalid_transition() => {
                self.pending_question = None;
                self.phase = Phase::Streaming;
            }
            Err(err) => tracing::warn!(error = %err, "recording task.input_provided failed"),
        }
    }

    // -- terminal ----------------------------------------------------------

    fn validate(
        &mut self,
        text: &str,
        structured: Option<serde_json::Value>,
        transcript: Option<ArtifactRef>,
    ) -> AttemptResult {
        self.phase = Phase::Validating;
        self.artifacts = transcript.into_iter().collect();
        let Some(schema) = self.attempt.spec.output_schema.clone() else {
            self.summary = summarise(text);
            return AttemptResult::Succeeded;
        };
        let validated = match structured {
            Some(value) => structured::validate(&value, &schema).map(|()| value),
            None => structured::extract_and_validate(text, &schema),
        };
        match validated {
            Ok(_) => {
                self.summary = summarise(text);
                AttemptResult::Succeeded
            }
            Err(err) => AttemptResult::Failed {
                class: FailureClass::Permanent,
                message: format!("output schema violation: {err}"),
            },
        }
    }

    async fn record_terminal(&mut self, result: AttemptResult) -> AttemptResult {
        let ctx = self.ctx();
        let recorded = match &result {
            AttemptResult::Succeeded => {
                self.deps
                    .tasks
                    .succeed_attempt(
                        self.attempt.task_id,
                        SucceedAttempt {
                            attempt_id: self.attempt.attempt_id,
                            artifacts: self.artifacts.clone(),
                            usage: self.usage,
                            summary: self.summary.clone(),
                        },
                        &ctx,
                    )
                    .await
            }
            AttemptResult::Failed { class, message } => {
                self.deps
                    .tasks
                    .fail_attempt(
                        self.attempt.task_id,
                        FailAttempt {
                            attempt_id: self.attempt.attempt_id,
                            class: *class,
                            message: message.clone(),
                            usage: self.usage,
                        },
                        &ctx,
                    )
                    .await
            }
        };
        match recorded {
            Ok(_) => result,
            Err(err) => {
                tracing::warn!(error = %err, "recording the terminal attempt event failed");
                result
            }
        }
    }

    async fn cleanup_workspace(&self, succeeded: bool) {
        if let Err(err) = self
            .deps
            .workspace
            .cleanup(&self.attempt.workspace, succeeded)
            .await
        {
            tracing::warn!(error = %err, "workspace cleanup failed");
        }
    }

    fn finalize_cost(&mut self) {
        let mut worker_usage = convert::usage_to_worker(&self.usage);
        finalize_cost(
            &mut worker_usage,
            &self.attempt.route.model,
            self.deps.prices.as_ref(),
        );
        self.usage = convert::usage_from_worker(&worker_usage);
    }

    fn record_metrics(&self, result: &AttemptResult, elapsed: Duration) {
        let outcome = result
            .failure_class()
            .map_or("succeeded", FailureClass::as_str);
        metrics::counter!(
            metric_names::TASK_ATTEMPTS_TOTAL,
            "kind" => self.attempt.kind.name().to_owned(),
            "worker" => self.attempt.route.worker.as_str(),
            "model_alias" => self.attempt.route.model.as_str().to_owned(),
            "outcome" => outcome,
        )
        .increment(1);
        metrics::histogram!(
            metric_names::TASK_ATTEMPT_DURATION_SECONDS,
            "kind" => self.attempt.kind.name().to_owned(),
            "worker" => self.attempt.route.worker.as_str(),
            "model_alias" => self.attempt.route.model.as_str().to_owned(),
        )
        .record(elapsed.as_secs_f64());
        metrics::counter!(
            metric_names::TOKENS_TOTAL,
            "model_alias" => self.attempt.route.model.as_str().to_owned(),
            "direction" => "input",
        )
        .increment(self.usage.input_tokens);
        metrics::counter!(
            metric_names::TOKENS_TOTAL,
            "model_alias" => self.attempt.route.model.as_str().to_owned(),
            "direction" => "output",
        )
        .increment(self.usage.output_tokens);
        // `kevin_cost_usd_total` is a *float* counter in `plan/10` and the
        // `metrics` facade has no float counter; the cost ledger projection
        // (WS-11) is the source of truth for spend and `kevin cost`.
    }

    fn ctx(&self) -> CommandContext {
        CommandContext::system(self.deps.ids.as_ref(), self.attempt.run_id)
    }
}

fn summarise(text: &str) -> String {
    let trimmed = text.trim();
    let mut summary: String = trimmed.chars().take(200).collect();
    if trimmed.chars().count() > 200 {
        summary.push('…');
    }
    summary
}

fn answer_text(answer: &Answer) -> String {
    if answer.selected.is_empty() {
        answer.free_text.clone().unwrap_or_default()
    } else {
        answer.selected.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_bounded() {
        let long = "x".repeat(500);
        let summary = summarise(&long);
        assert_eq!(summary.chars().count(), 201);
        assert_eq!(summarise("  hi  "), "hi");
    }

    #[test]
    fn answer_text_prefers_selected_labels() {
        assert_eq!(answer_text(&Answer::selected(["a", "b"], "user")), "a, b");
        assert_eq!(answer_text(&Answer::free_text("hello", "user")), "hello");
    }

    #[test]
    fn phase_names_are_snake_case() {
        assert_eq!(Phase::AwaitingInput.as_str(), "awaiting_input");
        assert_eq!(Phase::Starting.as_str(), "starting");
    }
}
