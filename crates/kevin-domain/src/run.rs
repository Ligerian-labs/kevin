//! The [`Run`] aggregate root (`plan/02-domain-model.md` §Aggregates › Run).
//!
//! State machine (`RunStatus`):
//!
//! ```text
//! received ──StartUnderstanding──▶ understanding ──RecordUnderstanding──▶ awaiting_answers
//!                                                            │ (no questions)
//! awaiting_answers ──NoteQuestionAnswered (last)──▶ planning ◀┘
//! planning ──ProposePlan──▶ awaiting_plan_approval ──ApprovePlan──▶ executing
//! planning ──ProposePlan──▶ (headless/kohral or auto_approve) plan_approved{by: auto} ──▶ executing
//! awaiting_plan_approval ──RejectPlan──▶ planning
//! executing ──NoteTaskTerminal (all terminal)──▶ integrating ──MarkIntegrated──▶ evaluating
//! evaluating ──MarkEvaluated──▶ completed
//! any non-terminal ──CancelRun──▶ cancelled
//! any non-terminal ──FailRun──▶ failed
//! ```
//!
//! Commands are plain structs wrapped by [`RunCommand`]; events are the
//! variants of [`RunEvent`], serialised internally tagged on `type` with the
//! exact catalog names (`run.started`, …).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::ids::{EvaluationId, QuestionId, RunId, TaskId};
use crate::kinds::FailureClass;
use crate::kinds::ModelAlias;
use crate::plan::{Plan, PlanValidator};
use crate::understanding::Understanding;
use crate::values::{
    ArtifactRef, Budget, BudgetDimension, BudgetExcess, Goal, Route, RunFailureReason, RunMode,
    Usage, Verdict,
};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const RUN_AGGREGATE_TYPE: &str = "run";

/// Per-run override of the `[roles]` table, keyed by role name
/// (`planner`, `clarifier`, `judge`, `integrator`, `default`).
///
/// A `String` key rather than an enum: the role enum belongs to
/// `kevin-config`, and `kevin-domain` depends on nothing internal
/// (`plan/01-architecture.md` §Dependency direction). An unknown key is
/// simply never looked up.
pub type RoleOverrides = BTreeMap<String, ModelAlias>;

/// Value of `by` in `run.plan_approved` for automatic approvals.
pub const AUTO_APPROVED_BY: &str = "auto";

// ---------------------------------------------------------------------------
// Status & outcomes
// ---------------------------------------------------------------------------

/// Lifecycle state of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// `run.started` applied; waiting for the planner call.
    #[default]
    Received,
    /// Planner is producing the understanding.
    Understanding,
    /// Clarification questions are open.
    AwaitingAnswers,
    /// Planner is producing (or revising) the plan.
    Planning,
    /// Interactive run waiting for a human to approve the plan.
    AwaitingPlanApproval,
    /// Tasks are scheduled and running.
    Executing,
    /// Integrator is merging / opening PRs.
    Integrating,
    /// Judge is evaluating the integrated result.
    Evaluating,
    /// Terminal: success.
    Completed,
    /// Terminal: failure (`run.failed.reason`).
    Failed,
    /// Terminal: cancelled by a user or the runtime.
    Cancelled,
}

impl RunStatus {
    /// Every status in lifecycle order.
    pub const ALL: [RunStatus; 11] = [
        RunStatus::Received,
        RunStatus::Understanding,
        RunStatus::AwaitingAnswers,
        RunStatus::Planning,
        RunStatus::AwaitingPlanApproval,
        RunStatus::Executing,
        RunStatus::Integrating,
        RunStatus::Evaluating,
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            RunStatus::Received => "received",
            RunStatus::Understanding => "understanding",
            RunStatus::AwaitingAnswers => "awaiting_answers",
            RunStatus::Planning => "planning",
            RunStatus::AwaitingPlanApproval => "awaiting_plan_approval",
            RunStatus::Executing => "executing",
            RunStatus::Integrating => "integrating",
            RunStatus::Evaluating => "evaluating",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
        }
    }

    /// `completed`, `failed` or `cancelled`.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a task ended, as noted on the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    /// `task.attempt_succeeded`.
    Succeeded,
    /// Attempts exhausted or permanent failure.
    Failed,
    /// `task.cancelled`.
    Cancelled,
    /// `task.skipped`.
    Skipped,
}

impl TaskOutcome {
    /// `true` for [`TaskOutcome::Succeeded`].
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, TaskOutcome::Succeeded)
    }
}

/// Outcome of the run-level evaluation carried by `MarkEvaluated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvaluation {
    /// The evaluation aggregate.
    pub evaluation_id: EvaluationId,
    /// Overall score 0..=1.
    pub overall: f32,
    /// Verdict.
    pub verdict: Verdict,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Creates the run (`run.started`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartRun {
    /// Id of the new run.
    pub run_id: RunId,
    /// What to do.
    pub goal: Goal,
    /// Interaction mode.
    pub mode: RunMode,
    /// Effective budget (config defaults merged with overrides).
    pub budget: Budget,
    /// Who asked.
    pub requested_by: String,
    /// `kevin.auto_approve_plans`; headless/Kohral modes auto-approve regardless.
    #[serde(default)]
    pub auto_approve_plans: bool,
    /// Per-run override of the `[roles]` table (`plan/02` §Run).
    ///
    /// Keyed by role name — `planner`, `clarifier`, `judge`, `integrator`,
    /// `default` — because the role enum lives in `kevin-config`, which this
    /// crate must not depend on. Empty for a CLI or API run; Kohral fills it
    /// from the turn's `model` field, the only way a caller picks the model for
    /// one run without editing configuration.
    #[serde(default)]
    pub role_overrides: RoleOverrides,
}

/// The planner call started (`run.understanding_started`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartUnderstanding {
    /// Route used for the planner role.
    pub planner_route: Route,
}

/// Records the planner's understanding (`run.understanding_completed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordUnderstanding {
    /// The understanding.
    pub understanding: Understanding,
    /// Planner call usage.
    pub usage: Usage,
    /// Ids of the `Question`s the saga will ask (already selected by
    /// threshold); empty → straight to planning.
    #[serde(default)]
    pub question_ids: Vec<QuestionId>,
}

/// A clarification question of this run was answered (from the process manager).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteQuestionAnswered {
    /// The question.
    pub question_id: QuestionId,
}

/// Records a plan proposal (`run.plan_proposed`, plus `run.plan_approved{by: auto}`
/// when the run auto-approves).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposePlan {
    /// The plan (validated with [`PlanValidator`]).
    pub plan: Plan,
    /// Planner call usage.
    pub usage: Usage,
    /// `orchestrator.max_tasks_per_run`.
    pub max_tasks: usize,
}

impl ProposePlan {
    /// A proposal validated with the default task limit.
    #[must_use]
    pub fn new(plan: Plan, usage: Usage) -> Self {
        Self {
            plan,
            usage,
            max_tasks: crate::plan::DEFAULT_MAX_TASKS,
        }
    }
}

/// A human approves the proposed plan (`run.plan_approved`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovePlan {
    /// Who approved.
    pub by: String,
}

/// A human rejects the proposed plan (`run.plan_rejected`); the run re-plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectPlan {
    /// Who rejected.
    pub by: String,
    /// Feedback for the next planning call.
    pub feedback: String,
}

/// Tasks were created for the approved plan (`run.execution_started`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartExecution {
    /// Task ids in plan order.
    pub task_ids: Vec<TaskId>,
}

/// Reports a task's cumulative usage so the run can enforce its budget
/// (`run.usage_recorded`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordTaskUsage {
    /// The task (plan task, integration task or judge call).
    pub task_id: TaskId,
    /// The task's cumulative usage (replaces the previous value).
    pub usage: Usage,
}

/// A task reached a terminal state (`run.task_terminal_noted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteTaskTerminal {
    /// The task.
    pub task_id: TaskId,
    /// How it ended.
    pub outcome: TaskOutcome,
    /// Its final cumulative usage.
    pub usage: Usage,
}

/// The orchestrator detected an exhausted budget dimension the run cannot
/// observe itself (wall-clock) (`run.budget_exhausted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExhaustBudget {
    /// What was exceeded.
    pub excess: BudgetExcess,
}

/// Integration finished (`run.integrated`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkIntegrated {
    /// PR URLs, final diff, branch names.
    pub artifacts: Vec<ArtifactRef>,
    /// Human-readable summary.
    pub summary: String,
}

/// The run-level evaluation finished (`run.evaluated` + `run.completed`), or
/// was skipped (`run.completed` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarkEvaluated {
    /// The evaluation, or `None` when it timed out / was skipped.
    pub evaluation: Option<RunEvaluation>,
    /// Final run summary.
    pub summary: String,
}

/// Cancels the run (`run.cancelled`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRun {
    /// Who cancelled.
    pub by: String,
    /// Why.
    pub reason: String,
}

/// Fails the run (`run.failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailRun {
    /// Machine-readable reason.
    pub reason: RunFailureReason,
    /// Failure class.
    pub class: FailureClass,
    /// Optional detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Requests a (re-)evaluation of a terminal run (`run.evaluation_requested`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evaluate {
    /// Who asked.
    pub requested_by: String,
}

/// Every command the [`Run`] aggregate handles.
#[derive(Debug, Clone, PartialEq)]
pub enum RunCommand {
    /// [`StartRun`].
    Start(StartRun),
    /// [`StartUnderstanding`].
    StartUnderstanding(StartUnderstanding),
    /// [`RecordUnderstanding`].
    RecordUnderstanding(RecordUnderstanding),
    /// [`NoteQuestionAnswered`].
    NoteQuestionAnswered(NoteQuestionAnswered),
    /// [`ProposePlan`].
    ProposePlan(ProposePlan),
    /// [`ApprovePlan`].
    ApprovePlan(ApprovePlan),
    /// [`RejectPlan`].
    RejectPlan(RejectPlan),
    /// [`StartExecution`].
    StartExecution(StartExecution),
    /// [`RecordTaskUsage`].
    RecordTaskUsage(RecordTaskUsage),
    /// [`NoteTaskTerminal`].
    NoteTaskTerminal(NoteTaskTerminal),
    /// [`ExhaustBudget`].
    ExhaustBudget(ExhaustBudget),
    /// [`MarkIntegrated`].
    MarkIntegrated(MarkIntegrated),
    /// [`MarkEvaluated`].
    MarkEvaluated(MarkEvaluated),
    /// [`CancelRun`].
    Cancel(CancelRun),
    /// [`FailRun`].
    Fail(FailRun),
    /// [`Evaluate`].
    Evaluate(Evaluate),
}

impl RunCommand {
    /// `snake_case` command name (used in `DomainError::InvalidTransition`).
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            RunCommand::Start(_) => "start_run",
            RunCommand::StartUnderstanding(_) => "start_understanding",
            RunCommand::RecordUnderstanding(_) => "record_understanding",
            RunCommand::NoteQuestionAnswered(_) => "note_question_answered",
            RunCommand::ProposePlan(_) => "propose_plan",
            RunCommand::ApprovePlan(_) => "approve_plan",
            RunCommand::RejectPlan(_) => "reject_plan",
            RunCommand::StartExecution(_) => "start_execution",
            RunCommand::RecordTaskUsage(_) => "record_task_usage",
            RunCommand::NoteTaskTerminal(_) => "note_task_terminal",
            RunCommand::ExhaustBudget(_) => "exhaust_budget",
            RunCommand::MarkIntegrated(_) => "mark_integrated",
            RunCommand::MarkEvaluated(_) => "mark_evaluated",
            RunCommand::Cancel(_) => "cancel_run",
            RunCommand::Fail(_) => "fail_run",
            RunCommand::Evaluate(_) => "evaluate",
        }
    }
}

macro_rules! command_from {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        $(impl From<$ty> for RunCommand {
            fn from(cmd: $ty) -> Self {
                RunCommand::$variant(cmd)
            }
        })*
    };
}

command_from!(
    Start(StartRun),
    StartUnderstanding(StartUnderstanding),
    RecordUnderstanding(RecordUnderstanding),
    NoteQuestionAnswered(NoteQuestionAnswered),
    ProposePlan(ProposePlan),
    ApprovePlan(ApprovePlan),
    RejectPlan(RejectPlan),
    StartExecution(StartExecution),
    RecordTaskUsage(RecordTaskUsage),
    NoteTaskTerminal(NoteTaskTerminal),
    ExhaustBudget(ExhaustBudget),
    MarkIntegrated(MarkIntegrated),
    MarkEvaluated(MarkEvaluated),
    Cancel(CancelRun),
    Fail(FailRun),
    Evaluate(Evaluate),
);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `run` stream. Serde: internally tagged on `type` with the
/// catalog name, payload fields inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RunEvent {
    /// `run.started`
    #[serde(rename = "run.started")]
    Started {
        /// The run id.
        run_id: RunId,
        /// Goal.
        goal: Goal,
        /// Mode.
        mode: RunMode,
        /// Budget.
        budget: Budget,
        /// Who asked.
        requested_by: String,
        /// Plans auto-approve (config or mode).
        auto_approve_plans: bool,
        /// Per-run `[roles]` override, recorded so a restarted runtime resumes
        /// the run on the model the caller asked for.
        #[serde(default)]
        role_overrides: RoleOverrides,
    },
    /// `run.understanding_started`
    #[serde(rename = "run.understanding_started")]
    UnderstandingStarted {
        /// Planner route.
        planner_route: Route,
    },
    /// `run.understanding_completed`
    #[serde(rename = "run.understanding_completed")]
    UnderstandingCompleted {
        /// The understanding.
        understanding: Understanding,
        /// Planner usage.
        usage: Usage,
        /// Questions that will be asked.
        question_ids: Vec<QuestionId>,
    },
    /// `run.question_answered` — a clarification question of this run was answered.
    #[serde(rename = "run.question_answered")]
    QuestionAnswered {
        /// The question.
        question_id: QuestionId,
        /// Open questions left after this one.
        remaining_open: u32,
    },
    /// `run.plan_proposed`
    #[serde(rename = "run.plan_proposed")]
    PlanProposed {
        /// The plan.
        plan: Plan,
        /// Planner usage.
        usage: Usage,
        /// 0 for the first proposal, +1 per rejection.
        revision: u8,
    },
    /// `run.plan_approved`
    #[serde(rename = "run.plan_approved")]
    PlanApproved {
        /// Who approved (`auto` when automatic).
        by: String,
    },
    /// `run.plan_rejected`
    #[serde(rename = "run.plan_rejected")]
    PlanRejected {
        /// Who rejected.
        by: String,
        /// Feedback for re-planning.
        feedback: String,
    },
    /// `run.execution_started`
    #[serde(rename = "run.execution_started")]
    ExecutionStarted {
        /// Task ids in plan order.
        task_ids: Vec<TaskId>,
    },
    /// `run.usage_recorded` — a task's cumulative usage was rolled up.
    #[serde(rename = "run.usage_recorded")]
    UsageRecorded {
        /// The task.
        task_id: TaskId,
        /// Its cumulative usage.
        task_usage: Usage,
        /// Run total after the update.
        run_usage: Usage,
    },
    /// `run.task_terminal_noted` — a task ended.
    #[serde(rename = "run.task_terminal_noted")]
    TaskTerminalNoted {
        /// The task.
        task_id: TaskId,
        /// How it ended.
        outcome: TaskOutcome,
        /// Its final usage.
        task_usage: Usage,
        /// Run total after the update.
        run_usage: Usage,
        /// Every plan task is now terminal.
        all_terminal: bool,
    },
    /// `run.budget_exhausted`
    #[serde(rename = "run.budget_exhausted")]
    BudgetExhausted {
        /// Which limit.
        dimension: BudgetDimension,
        /// The limit.
        limit: rust_decimal::Decimal,
        /// The observed value.
        actual: rust_decimal::Decimal,
    },
    /// `run.integrated`
    #[serde(rename = "run.integrated")]
    Integrated {
        /// Integration artifacts.
        artifacts: Vec<ArtifactRef>,
        /// Summary.
        summary: String,
    },
    /// `run.evaluated`
    #[serde(rename = "run.evaluated")]
    Evaluated {
        /// The evaluation.
        evaluation_id: EvaluationId,
        /// Overall score.
        overall: f32,
        /// Verdict.
        verdict: Verdict,
    },
    /// `run.completed`
    #[serde(rename = "run.completed")]
    Completed {
        /// Final summary.
        summary: String,
        /// Final usage.
        usage: Usage,
        /// The evaluation was skipped (timeout).
        evaluation_skipped: bool,
    },
    /// `run.failed`
    #[serde(rename = "run.failed")]
    Failed {
        /// Reason.
        reason: RunFailureReason,
        /// Class.
        class: FailureClass,
        /// Usage at failure.
        usage: Usage,
        /// Detail.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// `run.cancelled`
    #[serde(rename = "run.cancelled")]
    Cancelled {
        /// Who.
        by: String,
        /// Why.
        reason: String,
    },
    /// `run.evaluation_requested` — re-evaluation of a terminal run.
    #[serde(rename = "run.evaluation_requested")]
    EvaluationRequested {
        /// Who asked.
        requested_by: String,
    },
}

impl RunEvent {
    /// Every event type of the `run` stream, in catalog order.
    pub const TYPES: [&'static str; 17] = [
        "run.started",
        "run.understanding_started",
        "run.understanding_completed",
        "run.question_answered",
        "run.plan_proposed",
        "run.plan_approved",
        "run.plan_rejected",
        "run.execution_started",
        "run.usage_recorded",
        "run.task_terminal_noted",
        "run.budget_exhausted",
        "run.integrated",
        "run.evaluated",
        "run.completed",
        "run.failed",
        "run.cancelled",
        "run.evaluation_requested",
    ];
}

impl EventMeta for RunEvent {
    fn event_type(&self) -> &'static str {
        match self {
            RunEvent::Started { .. } => "run.started",
            RunEvent::UnderstandingStarted { .. } => "run.understanding_started",
            RunEvent::UnderstandingCompleted { .. } => "run.understanding_completed",
            RunEvent::QuestionAnswered { .. } => "run.question_answered",
            RunEvent::PlanProposed { .. } => "run.plan_proposed",
            RunEvent::PlanApproved { .. } => "run.plan_approved",
            RunEvent::PlanRejected { .. } => "run.plan_rejected",
            RunEvent::ExecutionStarted { .. } => "run.execution_started",
            RunEvent::UsageRecorded { .. } => "run.usage_recorded",
            RunEvent::TaskTerminalNoted { .. } => "run.task_terminal_noted",
            RunEvent::BudgetExhausted { .. } => "run.budget_exhausted",
            RunEvent::Integrated { .. } => "run.integrated",
            RunEvent::Evaluated { .. } => "run.evaluated",
            RunEvent::Completed { .. } => "run.completed",
            RunEvent::Failed { .. } => "run.failed",
            RunEvent::Cancelled { .. } => "run.cancelled",
            RunEvent::EvaluationRequested { .. } => "run.evaluation_requested",
        }
    }

    fn schema_version(&self) -> u16 {
        1
    }

    fn aggregate_type(&self) -> &'static str {
        RUN_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The run aggregate root.
#[derive(Debug, Clone)]
pub struct Run {
    version: u64,
    id: RunId,
    goal: Option<Goal>,
    requested_by: String,
    mode: Option<RunMode>,
    budget: Budget,
    auto_approve_plans: bool,
    role_overrides: RoleOverrides,
    status: RunStatus,
    understanding: Option<Understanding>,
    plan: Option<Plan>,
    plan_approved: bool,
    plan_revisions: u8,
    task_ids: Vec<TaskId>,
    task_outcomes: BTreeMap<TaskId, TaskOutcome>,
    open_question_ids: Vec<QuestionId>,
    overhead_usage: Usage,
    task_usage: BTreeMap<TaskId, Usage>,
    usage: Usage,
    budget_exhausted: Option<BudgetExcess>,
    artifacts: Vec<ArtifactRef>,
    evaluation_ids: Vec<EvaluationId>,
    failure: Option<RunFailureReason>,
}

impl Default for Run {
    fn default() -> Self {
        Self {
            version: 0,
            id: RunId::nil(),
            goal: None,
            requested_by: String::new(),
            mode: None,
            budget: Budget::unlimited(),
            auto_approve_plans: false,
            role_overrides: RoleOverrides::new(),
            status: RunStatus::Received,
            understanding: None,
            plan: None,
            plan_approved: false,
            plan_revisions: 0,
            task_ids: Vec::new(),
            task_outcomes: BTreeMap::new(),
            open_question_ids: Vec::new(),
            overhead_usage: Usage::ZERO,
            task_usage: BTreeMap::new(),
            usage: Usage::ZERO,
            budget_exhausted: None,
            artifacts: Vec::new(),
            evaluation_ids: Vec::new(),
            failure: None,
        }
    }
}

impl Run {
    /// Typed id.
    #[must_use]
    pub const fn run_id(&self) -> RunId {
        self.id
    }

    /// Current status.
    #[must_use]
    pub const fn status(&self) -> RunStatus {
        self.status
    }

    /// The goal (after `run.started`).
    #[must_use]
    pub const fn goal(&self) -> Option<&Goal> {
        self.goal.as_ref()
    }

    /// Who requested the run.
    #[must_use]
    pub fn requested_by(&self) -> &str {
        &self.requested_by
    }

    /// The mode (after `run.started`).
    #[must_use]
    pub const fn mode(&self) -> Option<&RunMode> {
        self.mode.as_ref()
    }

    /// The budget.
    #[must_use]
    pub const fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Per-run `[roles]` overrides given at [`StartRun`] (empty by default).
    #[must_use]
    pub const fn role_overrides(&self) -> &RoleOverrides {
        &self.role_overrides
    }

    /// Rolled-up usage (planner calls + every task's cumulative usage).
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Usage recorded per task.
    #[must_use]
    pub const fn task_usage(&self) -> &BTreeMap<TaskId, Usage> {
        &self.task_usage
    }

    /// The understanding, once recorded.
    #[must_use]
    pub const fn understanding(&self) -> Option<&Understanding> {
        self.understanding.as_ref()
    }

    /// The latest proposed plan.
    #[must_use]
    pub const fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// Whether the latest plan was approved.
    #[must_use]
    pub const fn plan_approved(&self) -> bool {
        self.plan_approved
    }

    /// Number of rejected plans so far.
    #[must_use]
    pub const fn plan_revisions(&self) -> u8 {
        self.plan_revisions
    }

    /// Plans auto-approve (headless/Kohral mode or `auto_approve_plans`).
    #[must_use]
    pub fn auto_approves_plans(&self) -> bool {
        self.auto_approve_plans || self.mode.as_ref().is_some_and(|m| !m.is_interactive())
    }

    /// Task ids of the approved plan (after `run.execution_started`).
    #[must_use]
    pub fn task_ids(&self) -> &[TaskId] {
        &self.task_ids
    }

    /// Outcomes of terminal plan tasks.
    #[must_use]
    pub const fn task_outcomes(&self) -> &BTreeMap<TaskId, TaskOutcome> {
        &self.task_outcomes
    }

    /// Clarification questions still open.
    #[must_use]
    pub fn open_question_ids(&self) -> &[QuestionId] {
        &self.open_question_ids
    }

    /// The crossed budget limit, if any.
    #[must_use]
    pub const fn budget_exhausted(&self) -> Option<&BudgetExcess> {
        self.budget_exhausted.as_ref()
    }

    /// Integration artifacts.
    #[must_use]
    pub fn artifacts(&self) -> &[ArtifactRef] {
        &self.artifacts
    }

    /// Evaluations recorded for the run.
    #[must_use]
    pub fn evaluation_ids(&self) -> &[EvaluationId] {
        &self.evaluation_ids
    }

    /// Failure reason after `run.failed`.
    #[must_use]
    pub const fn failure(&self) -> Option<&RunFailureReason> {
        self.failure.as_ref()
    }

    /// `true` once the status is terminal.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Every plan task has an outcome.
    #[must_use]
    pub fn all_tasks_terminal(&self) -> bool {
        !self.task_ids.is_empty()
            && self
                .task_ids
                .iter()
                .all(|id| self.task_outcomes.contains_key(id))
    }

    // -- helpers -----------------------------------------------------------

    fn reject(&self, cmd: &RunCommand) -> DomainError {
        DomainError::invalid_transition(RUN_AGGREGATE_TYPE, self.status.as_str(), cmd.name())
    }

    fn require_exists(&self, cmd: &RunCommand) -> Result<(), DomainError> {
        if self.version == 0 {
            return Err(DomainError::NotFound {
                aggregate: RUN_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if self.status.is_terminal() {
            return Err(self.reject(cmd));
        }
        Ok(())
    }

    fn require_status(&self, cmd: &RunCommand, allowed: &[RunStatus]) -> Result<(), DomainError> {
        self.require_exists(cmd)?;
        if allowed.contains(&self.status) {
            Ok(())
        } else {
            Err(self.reject(cmd))
        }
    }

    /// Run usage if `task_id`'s usage were replaced by `usage`.
    fn usage_with_task(&self, task_id: TaskId, usage: Usage) -> Usage {
        let tasks: Usage = self
            .task_usage
            .iter()
            .filter(|(id, _)| **id != task_id)
            .map(|(_, u)| *u)
            .sum();
        self.overhead_usage + tasks + usage
    }

    /// `run.budget_exhausted` if `usage` crosses a limit not yet reported.
    fn budget_event(&self, usage: &Usage) -> Option<RunEvent> {
        if self.budget_exhausted.is_some() {
            return None;
        }
        self.budget
            .exceeded_by(usage)
            .map(|e| RunEvent::BudgetExhausted {
                dimension: e.dimension,
                limit: e.limit,
                actual: e.actual,
            })
    }

    fn handle_start(&self, cmd: &StartRun) -> Result<Vec<RunEvent>, DomainError> {
        if self.version > 0 {
            return Err(DomainError::AlreadyExists {
                aggregate: RUN_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if cmd.goal.text.trim().is_empty() {
            return Err(DomainError::invalid_value("goal.text", "must not be empty"));
        }
        if cmd.budget.max_attempts == 0 {
            return Err(DomainError::invalid_value(
                "budget.max_attempts",
                "must be at least 1",
            ));
        }
        if cmd.budget.max_parallel == 0 {
            return Err(DomainError::invalid_value(
                "budget.max_parallel",
                "must be at least 1",
            ));
        }
        Ok(vec![RunEvent::Started {
            run_id: cmd.run_id,
            goal: cmd.goal.clone(),
            mode: cmd.mode.clone(),
            budget: cmd.budget.clone(),
            requested_by: cmd.requested_by.clone(),
            auto_approve_plans: cmd.auto_approve_plans,
            role_overrides: cmd.role_overrides.clone(),
        }])
    }

    fn handle_record_understanding(
        &self,
        cmd: &RecordUnderstanding,
    ) -> Result<Vec<RunEvent>, DomainError> {
        cmd.understanding.validate()?;
        let mut question_ids = cmd.question_ids.clone();
        question_ids.dedup();
        let mut events = vec![RunEvent::UnderstandingCompleted {
            understanding: cmd.understanding.clone(),
            usage: cmd.usage,
            question_ids,
        }];
        let total = self.usage + cmd.usage;
        events.extend(self.budget_event(&total));
        Ok(events)
    }

    fn handle_propose_plan(&self, cmd: &ProposePlan) -> Result<Vec<RunEvent>, DomainError> {
        PlanValidator::new(cmd.max_tasks)
            .validate(&cmd.plan)
            .map_err(DomainError::InvalidPlan)?;
        let mut events = vec![RunEvent::PlanProposed {
            plan: cmd.plan.clone(),
            usage: cmd.usage,
            revision: self.plan_revisions,
        }];
        if self.auto_approves_plans() {
            events.push(RunEvent::PlanApproved {
                by: AUTO_APPROVED_BY.to_owned(),
            });
        }
        let total = self.usage + cmd.usage;
        events.extend(self.budget_event(&total));
        Ok(events)
    }

    fn handle_start_execution(&self, cmd: &StartExecution) -> Result<Vec<RunEvent>, DomainError> {
        if !self.plan_approved {
            return Err(DomainError::PlanRequired);
        }
        if !self.task_ids.is_empty() {
            return Err(DomainError::invalid_value(
                "task_ids",
                "execution already started",
            ));
        }
        let expected = self.plan.as_ref().map_or(0, |p| p.tasks.len());
        if cmd.task_ids.len() != expected {
            return Err(DomainError::invalid_value(
                "task_ids",
                format!(
                    "expected {expected} task ids (one per plan task), got {}",
                    cmd.task_ids.len()
                ),
            ));
        }
        let mut unique = cmd.task_ids.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != cmd.task_ids.len() {
            return Err(DomainError::invalid_value("task_ids", "duplicate task id"));
        }
        Ok(vec![RunEvent::ExecutionStarted {
            task_ids: cmd.task_ids.clone(),
        }])
    }

    fn handle_record_task_usage(
        &self,
        cmd: &RecordTaskUsage,
    ) -> Result<Vec<RunEvent>, DomainError> {
        if let Some(previous) = self.task_usage.get(&cmd.task_id)
            && !usage_covers(&cmd.usage, previous)
        {
            return Err(DomainError::invalid_value(
                "usage",
                "cumulative task usage must not decrease",
            ));
        }
        let run_usage = self.usage_with_task(cmd.task_id, cmd.usage);
        let mut events = vec![RunEvent::UsageRecorded {
            task_id: cmd.task_id,
            task_usage: cmd.usage,
            run_usage,
        }];
        events.extend(self.budget_event(&run_usage));
        Ok(events)
    }

    fn handle_note_task_terminal(
        &self,
        cmd: &NoteTaskTerminal,
    ) -> Result<Vec<RunEvent>, DomainError> {
        let is_plan_task = self.task_ids.contains(&cmd.task_id);
        if self.status == RunStatus::Executing && !is_plan_task {
            return Err(DomainError::UnknownTask {
                task_id: cmd.task_id,
            });
        }
        if self.task_outcomes.contains_key(&cmd.task_id) {
            return Err(DomainError::invalid_value(
                "task_id",
                "task outcome already noted",
            ));
        }
        if let Some(previous) = self.task_usage.get(&cmd.task_id)
            && !usage_covers(&cmd.usage, previous)
        {
            return Err(DomainError::invalid_value(
                "usage",
                "cumulative task usage must not decrease",
            ));
        }
        let run_usage = self.usage_with_task(cmd.task_id, cmd.usage);
        let all_terminal = is_plan_task
            && self
                .task_ids
                .iter()
                .all(|id| *id == cmd.task_id || self.task_outcomes.contains_key(id));
        let mut events = vec![RunEvent::TaskTerminalNoted {
            task_id: cmd.task_id,
            outcome: cmd.outcome,
            task_usage: cmd.usage,
            run_usage,
            all_terminal,
        }];
        events.extend(self.budget_event(&run_usage));
        Ok(events)
    }

    fn handle_mark_evaluated(&self, cmd: &MarkEvaluated) -> Result<Vec<RunEvent>, DomainError> {
        let mut events = Vec::new();
        if let Some(evaluation) = &cmd.evaluation {
            if !(0.0..=1.0).contains(&evaluation.overall) {
                return Err(DomainError::invalid_value(
                    "overall",
                    "must be within 0..=1",
                ));
            }
            events.push(RunEvent::Evaluated {
                evaluation_id: evaluation.evaluation_id,
                overall: evaluation.overall,
                verdict: evaluation.verdict,
            });
        }
        events.push(RunEvent::Completed {
            summary: cmd.summary.clone(),
            usage: self.usage,
            evaluation_skipped: cmd.evaluation.is_none(),
        });
        Ok(events)
    }

    fn handle_re_evaluated(cmd: &MarkEvaluated) -> Result<Vec<RunEvent>, DomainError> {
        let Some(evaluation) = &cmd.evaluation else {
            return Err(DomainError::invalid_value(
                "evaluation",
                "re-evaluation of a terminal run requires an evaluation",
            ));
        };
        if !(0.0..=1.0).contains(&evaluation.overall) {
            return Err(DomainError::invalid_value(
                "overall",
                "must be within 0..=1",
            ));
        }
        Ok(vec![RunEvent::Evaluated {
            evaluation_id: evaluation.evaluation_id,
            overall: evaluation.overall,
            verdict: evaluation.verdict,
        }])
    }
}

/// `new` is component-wise ≥ `old` (cumulative usage never shrinks).
fn usage_covers(new: &Usage, old: &Usage) -> bool {
    new.input_tokens >= old.input_tokens
        && new.output_tokens >= old.output_tokens
        && new.cache_read_tokens >= old.cache_read_tokens
        && new.cache_write_tokens >= old.cache_write_tokens
        && new.wall_ms >= old.wall_ms
        && match (new.cost_usd, old.cost_usd) {
            (Some(n), Some(o)) => n >= o,
            (None, Some(_)) => false,
            _ => true,
        }
}

impl Aggregate for Run {
    type Command = RunCommand;
    type Event = RunEvent;

    const TYPE: &'static str = RUN_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        self.id.as_uuid()
    }

    fn version(&self) -> u64 {
        self.version
    }

    #[allow(clippy::too_many_lines)]
    fn handle(&self, cmd: &RunCommand) -> Result<Vec<RunEvent>, DomainError> {
        use RunStatus as S;
        match cmd {
            RunCommand::Start(c) => self.handle_start(c),
            RunCommand::StartUnderstanding(c) => {
                self.require_status(cmd, &[S::Received])?;
                Ok(vec![RunEvent::UnderstandingStarted {
                    planner_route: c.planner_route.clone(),
                }])
            }
            RunCommand::RecordUnderstanding(c) => {
                self.require_status(cmd, &[S::Understanding])?;
                self.handle_record_understanding(c)
            }
            RunCommand::NoteQuestionAnswered(c) => {
                self.require_status(cmd, &[S::AwaitingAnswers])?;
                if !self.open_question_ids.contains(&c.question_id) {
                    return Err(DomainError::UnknownQuestion {
                        question_id: c.question_id,
                    });
                }
                let remaining = self.open_question_ids.len().saturating_sub(1);
                Ok(vec![RunEvent::QuestionAnswered {
                    question_id: c.question_id,
                    remaining_open: u32::try_from(remaining).unwrap_or(u32::MAX),
                }])
            }
            RunCommand::ProposePlan(c) => {
                self.require_status(cmd, &[S::Planning])?;
                self.handle_propose_plan(c)
            }
            RunCommand::ApprovePlan(c) => {
                self.require_status(cmd, &[S::AwaitingPlanApproval])?;
                Ok(vec![RunEvent::PlanApproved { by: c.by.clone() }])
            }
            RunCommand::RejectPlan(c) => {
                self.require_status(cmd, &[S::AwaitingPlanApproval])?;
                Ok(vec![RunEvent::PlanRejected {
                    by: c.by.clone(),
                    feedback: c.feedback.clone(),
                }])
            }
            RunCommand::StartExecution(c) => {
                self.require_status(cmd, &[S::Executing])?;
                self.handle_start_execution(c)
            }
            RunCommand::RecordTaskUsage(c) => {
                self.require_status(cmd, &[S::Executing, S::Integrating, S::Evaluating])?;
                self.handle_record_task_usage(c)
            }
            RunCommand::NoteTaskTerminal(c) => {
                self.require_status(cmd, &[S::Executing, S::Integrating])?;
                self.handle_note_task_terminal(c)
            }
            RunCommand::ExhaustBudget(c) => {
                self.require_exists(cmd)?;
                if self.budget_exhausted.is_some() {
                    return Err(self.reject(cmd));
                }
                Ok(vec![RunEvent::BudgetExhausted {
                    dimension: c.excess.dimension,
                    limit: c.excess.limit,
                    actual: c.excess.actual,
                }])
            }
            RunCommand::MarkIntegrated(c) => {
                self.require_status(cmd, &[S::Integrating])?;
                Ok(vec![RunEvent::Integrated {
                    artifacts: c.artifacts.clone(),
                    summary: c.summary.clone(),
                }])
            }
            RunCommand::MarkEvaluated(c) => {
                if self.version == 0 {
                    return Err(DomainError::NotFound {
                        aggregate: RUN_AGGREGATE_TYPE,
                        id: self.id.as_uuid(),
                    });
                }
                match self.status {
                    S::Evaluating => self.handle_mark_evaluated(c),
                    S::Completed | S::Failed | S::Cancelled => Self::handle_re_evaluated(c),
                    _ => Err(self.reject(cmd)),
                }
            }
            RunCommand::Cancel(c) => {
                self.require_exists(cmd)?;
                Ok(vec![RunEvent::Cancelled {
                    by: c.by.clone(),
                    reason: c.reason.clone(),
                }])
            }
            RunCommand::Fail(c) => {
                self.require_exists(cmd)?;
                Ok(vec![RunEvent::Failed {
                    reason: c.reason.clone(),
                    class: c.class,
                    usage: self.usage,
                    message: c.message.clone(),
                }])
            }
            RunCommand::Evaluate(c) => {
                if self.version == 0 {
                    return Err(DomainError::NotFound {
                        aggregate: RUN_AGGREGATE_TYPE,
                        id: self.id.as_uuid(),
                    });
                }
                if !self.status.is_terminal() {
                    return Err(self.reject(cmd));
                }
                Ok(vec![RunEvent::EvaluationRequested {
                    requested_by: c.requested_by.clone(),
                }])
            }
        }
    }

    fn apply(&mut self, event: &RunEvent) {
        self.version += 1;
        match event {
            RunEvent::Started {
                run_id,
                goal,
                mode,
                budget,
                requested_by,
                auto_approve_plans,
                role_overrides,
            } => {
                self.id = *run_id;
                self.goal = Some(goal.clone());
                self.mode = Some(mode.clone());
                self.budget = budget.clone();
                self.requested_by.clone_from(requested_by);
                self.auto_approve_plans = *auto_approve_plans;
                self.role_overrides.clone_from(role_overrides);
                self.status = RunStatus::Received;
            }
            RunEvent::UnderstandingStarted { .. } => {
                self.status = RunStatus::Understanding;
            }
            RunEvent::UnderstandingCompleted {
                understanding,
                usage,
                question_ids,
            } => {
                self.understanding = Some(understanding.clone());
                self.overhead_usage += *usage;
                self.usage += *usage;
                self.open_question_ids.clone_from(question_ids);
                self.status = if question_ids.is_empty() {
                    RunStatus::Planning
                } else {
                    RunStatus::AwaitingAnswers
                };
            }
            RunEvent::QuestionAnswered { question_id, .. } => {
                self.open_question_ids.retain(|q| q != question_id);
                if self.open_question_ids.is_empty() && self.status == RunStatus::AwaitingAnswers {
                    self.status = RunStatus::Planning;
                }
            }
            RunEvent::PlanProposed { plan, usage, .. } => {
                self.plan = Some(plan.clone());
                self.plan_approved = false;
                self.overhead_usage += *usage;
                self.usage += *usage;
                self.status = RunStatus::AwaitingPlanApproval;
            }
            RunEvent::PlanApproved { .. } => {
                self.plan_approved = true;
                self.status = RunStatus::Executing;
            }
            RunEvent::PlanRejected { .. } => {
                self.plan_approved = false;
                self.plan_revisions = self.plan_revisions.saturating_add(1);
                self.status = RunStatus::Planning;
            }
            RunEvent::ExecutionStarted { task_ids } => {
                self.task_ids.clone_from(task_ids);
            }
            RunEvent::UsageRecorded {
                task_id,
                task_usage,
                run_usage,
            } => {
                self.task_usage.insert(*task_id, *task_usage);
                self.usage = *run_usage;
            }
            RunEvent::TaskTerminalNoted {
                task_id,
                outcome,
                task_usage,
                run_usage,
                all_terminal,
            } => {
                self.task_usage.insert(*task_id, *task_usage);
                self.usage = *run_usage;
                self.task_outcomes.insert(*task_id, *outcome);
                if *all_terminal && self.status == RunStatus::Executing {
                    self.status = RunStatus::Integrating;
                }
            }
            RunEvent::BudgetExhausted {
                dimension,
                limit,
                actual,
            } => {
                self.budget_exhausted = Some(BudgetExcess {
                    dimension: *dimension,
                    limit: *limit,
                    actual: *actual,
                });
            }
            RunEvent::Integrated { artifacts, .. } => {
                self.artifacts.clone_from(artifacts);
                self.status = RunStatus::Evaluating;
            }
            RunEvent::Evaluated { evaluation_id, .. } => {
                self.evaluation_ids.push(*evaluation_id);
            }
            RunEvent::Completed { .. } => {
                self.status = RunStatus::Completed;
            }
            RunEvent::Failed { reason, .. } => {
                self.failure = Some(reason.clone());
                self.open_question_ids.clear();
                self.status = RunStatus::Failed;
            }
            RunEvent::Cancelled { .. } => {
                self.open_question_ids.clear();
                self.status = RunStatus::Cancelled;
            }
            RunEvent::EvaluationRequested { .. } => {}
        }
    }
}
