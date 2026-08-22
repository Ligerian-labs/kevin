//! Given/when/then helpers for aggregate tests (`plan/11-testing.md`
//! §Aggregate test helpers):
//!
//! ```ignore
//! given::<Run>(&[run::started(), run::understanding_started()])
//!     .when(RecordUnderstanding { .. })
//!     .then(&[RunEvent::UnderstandingCompleted { .. }]);   // exact events, ordered
//! given::<Run>(&[..]).when(..).then_err(DomainError::InvalidTransition { .. });
//! ```
//!
//! Also here: deterministic fixture ids, builders for every command/event with
//! sensible defaults ([`run`], [`task`], [`question`], [`evaluation`],
//! [`route_score`], [`memory_item`]) and the invariant checkers
//! [`assert_run_invariants`] / [`assert_task_invariants`] used by property tests.

use std::fmt::Debug;

use kevin_domain::aggregate::Aggregate;
use kevin_domain::error::DomainError;

/// Starts a scenario: rehydrates `A` from `events`.
pub fn given<A: Aggregate>(events: &[A::Event]) -> Given<A> {
    Given {
        aggregate: A::rehydrate(events),
    }
}

/// Starts a scenario on an aggregate that does not exist yet.
#[must_use]
pub fn given_nothing<A: Aggregate>() -> Given<A> {
    Given {
        aggregate: A::default(),
    }
}

/// The "given" phase: an aggregate rehydrated from history.
#[derive(Debug)]
pub struct Given<A: Aggregate> {
    aggregate: A,
}

impl<A: Aggregate> Given<A> {
    /// The rehydrated aggregate.
    pub const fn aggregate(&self) -> &A {
        &self.aggregate
    }

    /// Handles `cmd` (without applying).
    pub fn when(self, cmd: impl Into<A::Command>) -> When<A> {
        let cmd = cmd.into();
        let result = self.aggregate.handle(&cmd);
        When {
            aggregate: self.aggregate,
            cmd,
            result,
        }
    }
}

/// The "when" phase: the command's result, ready for assertions.
#[derive(Debug)]
pub struct When<A: Aggregate> {
    aggregate: A,
    cmd: A::Command,
    result: Result<Vec<A::Event>, DomainError>,
}

impl<A: Aggregate> When<A>
where
    A::Event: PartialEq + Debug,
{
    /// Asserts the command produced exactly `expected`, in order.
    #[track_caller]
    pub fn then(&self, expected: &[A::Event]) {
        match &self.result {
            Ok(events) => assert_eq!(
                events.as_slice(),
                expected,
                "command {:?} produced unexpected events",
                self.cmd
            ),
            Err(e) => panic!(
                "command {:?} was rejected with {e:?}, expected events {expected:?}",
                self.cmd
            ),
        }
    }

    /// Asserts the command was accepted; returns the events.
    #[track_caller]
    pub fn then_ok(&self) -> Vec<A::Event> {
        match &self.result {
            Ok(events) => events.clone(),
            Err(e) => panic!("command {:?} was rejected with {e:?}", self.cmd),
        }
    }

    /// Asserts the command was rejected with exactly `expected`.
    #[track_caller]
    #[allow(clippy::needless_pass_by_value)]
    pub fn then_err(&self, expected: DomainError) {
        match &self.result {
            Ok(events) => panic!(
                "command {:?} was accepted with {events:?}, expected error {expected:?}",
                self.cmd
            ),
            Err(e) => assert_eq!(
                e, &expected,
                "command {:?} rejected with the wrong error",
                self.cmd
            ),
        }
    }

    /// Asserts the command was rejected with an error matching `predicate`.
    #[track_caller]
    pub fn then_err_matching(&self, predicate: impl FnOnce(&DomainError) -> bool) {
        match &self.result {
            Ok(events) => panic!("command {:?} was accepted with {events:?}", self.cmd),
            Err(e) => assert!(
                predicate(e),
                "command {:?} rejected with unexpected error {e:?}",
                self.cmd
            ),
        }
    }

    /// Asserts the command was rejected with `DomainError::InvalidTransition`.
    #[track_caller]
    pub fn then_invalid_transition(&self) {
        self.then_err_matching(|e| matches!(e, DomainError::InvalidTransition { .. }));
    }

    /// Asserts the command was accepted, applies its events and lets the
    /// caller inspect the resulting state.
    #[track_caller]
    pub fn then_state(self, check: impl FnOnce(&A)) -> Vec<A::Event> {
        let events = self.then_ok();
        let mut aggregate = self.aggregate;
        for e in &events {
            aggregate.apply(e);
        }
        check(&aggregate);
        events
    }

    /// The raw result.
    pub const fn result(&self) -> &Result<Vec<A::Event>, DomainError> {
        &self.result
    }
}

// ---------------------------------------------------------------------------
// Fixture ids
// ---------------------------------------------------------------------------

/// Deterministic ids for fixtures and snapshots.
pub mod ids {
    use kevin_domain::ids::{
        ArtifactId, AttemptId, EvaluationId, MemoryItemId, ProposalId, QuestionId, RunId, TaskId,
    };
    use uuid::Uuid;

    const BASE: u128 = 0x0191_0000_0000_7000_8000_0000_0000_0000;

    /// `…-0000000000a1` style fixture uuid.
    #[must_use]
    pub const fn fixture_uuid(n: u128) -> Uuid {
        Uuid::from_u128(BASE | n)
    }

    /// Fixture run id.
    #[must_use]
    pub const fn run_id() -> RunId {
        RunId::from_uuid(fixture_uuid(0xa1))
    }
    /// Fixture task ids (`n` from 1).
    #[must_use]
    pub const fn task_id(n: u128) -> TaskId {
        TaskId::from_uuid(fixture_uuid(0xb00 + n))
    }
    /// Fixture attempt ids (`n` from 1).
    #[must_use]
    pub const fn attempt_id(n: u128) -> AttemptId {
        AttemptId::from_uuid(fixture_uuid(0xc00 + n))
    }
    /// Fixture question ids (`n` from 1).
    #[must_use]
    pub const fn question_id(n: u128) -> QuestionId {
        QuestionId::from_uuid(fixture_uuid(0xd00 + n))
    }
    /// Fixture evaluation id.
    #[must_use]
    pub const fn evaluation_id() -> EvaluationId {
        EvaluationId::from_uuid(fixture_uuid(0xe1))
    }
    /// Fixture proposal ids (`n` from 1).
    #[must_use]
    pub const fn proposal_id(n: u128) -> ProposalId {
        ProposalId::from_uuid(fixture_uuid(0xf00 + n))
    }
    /// Fixture memory item ids (`n` from 1).
    #[must_use]
    pub const fn memory_item_id(n: u128) -> MemoryItemId {
        MemoryItemId::from_uuid(fixture_uuid(0x1000 + n))
    }
    /// Fixture artifact ids (`n` from 1).
    #[must_use]
    pub const fn artifact_id(n: u128) -> ArtifactId {
        ArtifactId::from_uuid(fixture_uuid(0x2000 + n))
    }
}

/// Fixture timestamp `2026-01-01T00:00:00Z`.
#[must_use]
pub fn fixture_time() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_767_225_600, 0).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Shared value fixtures
// ---------------------------------------------------------------------------

/// Value-object fixtures.
pub mod values {
    use kevin_domain::kinds::{Effort, ModelAlias, WorkerKind};
    use kevin_domain::values::{
        ArtifactKind, ArtifactRef, Budget, Goal, RepoKind, Route, RubricScore, TaskSpec, Usage,
        Workspace,
    };
    use rust_decimal_shim::usd;

    /// Tiny re-export so fixtures can spell USD amounts without depending on
    /// `rust_decimal` directly.
    pub mod rust_decimal_shim {
        use kevin_domain::Decimal;

        /// Parses a USD amount (`"0.25"`).
        #[must_use]
        pub fn usd(s: &str) -> Decimal {
            s.parse().unwrap_or_default()
        }
    }

    /// `claude/sonnet5-claude@medium`.
    #[must_use]
    pub fn route() -> Route {
        Route::new(
            WorkerKind::Claude,
            ModelAlias::new("sonnet5-claude").unwrap_or_else(|e| panic!("{e}")),
        )
        .with_effort(Effort::Medium)
    }

    /// `codex/gpt56-codex`.
    #[must_use]
    pub fn other_route() -> Route {
        Route::new(
            WorkerKind::Codex,
            ModelAlias::new("gpt56-codex").unwrap_or_else(|e| panic!("{e}")),
        )
    }

    /// `claude/opus5-claude@xhigh` (planner/judge).
    #[must_use]
    pub fn planner_route() -> Route {
        Route::new(
            WorkerKind::Claude,
            ModelAlias::new("opus5-claude").unwrap_or_else(|e| panic!("{e}")),
        )
        .with_effort(Effort::XHigh)
    }

    /// Budget: 10 USD, 2 attempts, 4 parallel.
    #[must_use]
    pub fn budget() -> Budget {
        Budget::unlimited().with_max_usd(usd("10.0"))
    }

    /// A small usage record.
    #[must_use]
    pub fn usage() -> Usage {
        Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            cache_read_tokens: 50,
            cache_write_tokens: 10,
            cost_usd: Some(usd("0.25")),
            wall_ms: 1_500,
        }
    }

    /// A goal at `/repo`.
    #[must_use]
    pub fn goal() -> Goal {
        Goal::new("Add a /healthz endpoint", "/repo").with_repo_kind(RepoKind::Git)
    }

    /// A task spec.
    #[must_use]
    pub fn task_spec() -> TaskSpec {
        let mut spec = TaskSpec::new("Implement /healthz", "Add the route and a test.");
        spec.acceptance_criteria = vec!["GET /healthz returns 200".to_owned()];
        spec
    }

    /// An isolated workspace.
    #[must_use]
    pub fn workspace() -> Workspace {
        Workspace {
            root: "/repo/.kevin/workspaces/t1".into(),
            kind: kevin_domain::values::WorkspaceKind::GitWorktree {
                branch: "kevin/t1".to_owned(),
            },
            base_rev: Some("abc123".to_owned()),
        }
    }

    /// A diff artifact.
    #[must_use]
    pub fn artifact() -> ArtifactRef {
        ArtifactRef {
            id: super::ids::artifact_id(1),
            kind: ArtifactKind::Diff,
            uri: "artifact://diff-1".to_owned(),
            sha256: Some("deadbeef".to_owned()),
            bytes: Some(1_234),
        }
    }

    /// Two rubric scores.
    #[must_use]
    pub fn scores() -> Vec<RubricScore> {
        vec![
            RubricScore {
                criterion: "correctness".to_owned(),
                score: 9,
                rationale: "works".to_owned(),
            },
            RubricScore {
                criterion: "completeness".to_owned(),
                score: 7,
                rationale: "one criterion unverified".to_owned(),
            },
        ]
    }
}

// ---------------------------------------------------------------------------
// Run fixtures
// ---------------------------------------------------------------------------

/// `Run` command and event builders.
pub mod run {
    use kevin_domain::ids::{QuestionId, TaskId};
    use kevin_domain::kinds::{FailureClass, TaskKind};
    use kevin_domain::plan::{Plan, PlanTask};
    use kevin_domain::run::{
        AUTO_APPROVED_BY, ApprovePlan, CancelRun, Evaluate, ExhaustBudget, FailRun, MarkEvaluated,
        MarkIntegrated, NoteQuestionAnswered, NoteTaskTerminal, ProposePlan, RecordTaskUsage,
        RecordUnderstanding, RejectPlan, RunEvaluation, RunEvent, StartExecution, StartRun,
        StartUnderstanding, TaskOutcome,
    };
    use kevin_domain::understanding::Understanding;
    use kevin_domain::values::{
        BudgetDimension, BudgetExcess, RunFailureReason, RunMode, Usage, Verdict,
    };

    use super::ids;
    use super::values;

    /// A two-task plan (`t1 implement`, `t2 test` after `t1`).
    #[must_use]
    pub fn plan() -> Plan {
        Plan::new(
            vec![
                PlanTask::new("t1", "implement", "Implement /healthz"),
                PlanTask::new("t2", "test", "Test /healthz").depends_on(["t1"]),
            ],
            "small change, two steps",
        )
    }

    /// An understanding without questions.
    #[must_use]
    pub fn understanding() -> Understanding {
        let mut u = Understanding::new("Add a /healthz endpoint", "GET /healthz returns 200");
        u.suggested_task_kinds = vec![TaskKind::Implement.to_string(), TaskKind::Test.to_string()];
        u
    }

    /// Task ids for the two plan tasks.
    #[must_use]
    pub fn task_ids() -> Vec<TaskId> {
        vec![ids::task_id(1), ids::task_id(2)]
    }

    // -- commands ----------------------------------------------------------

    /// `StartRun` (interactive).
    #[must_use]
    pub fn start() -> StartRun {
        StartRun {
            run_id: ids::run_id(),
            goal: values::goal(),
            mode: RunMode::Interactive,
            budget: values::budget(),
            requested_by: "valentin".to_owned(),
            auto_approve_plans: false,
        }
    }

    /// `StartRun` in headless mode.
    #[must_use]
    pub fn start_headless() -> StartRun {
        StartRun {
            mode: RunMode::Headless,
            ..start()
        }
    }

    /// `StartRun` in Kohral mode.
    #[must_use]
    pub fn start_kohral() -> StartRun {
        StartRun {
            mode: RunMode::Kohral {
                turn_id: "turn-1".to_owned(),
                session_key: "sk".to_owned(),
                session_id: "sid".to_owned(),
            },
            ..start()
        }
    }

    /// `StartUnderstanding`.
    #[must_use]
    pub fn start_understanding() -> StartUnderstanding {
        StartUnderstanding {
            planner_route: values::planner_route(),
        }
    }

    /// `RecordUnderstanding` without questions.
    #[must_use]
    pub fn record_understanding() -> RecordUnderstanding {
        RecordUnderstanding {
            understanding: understanding(),
            usage: values::usage(),
            question_ids: Vec::new(),
        }
    }

    /// `RecordUnderstanding` with `question_ids`.
    #[must_use]
    pub fn record_understanding_with_questions(
        question_ids: Vec<QuestionId>,
    ) -> RecordUnderstanding {
        RecordUnderstanding {
            question_ids,
            ..record_understanding()
        }
    }

    /// `NoteQuestionAnswered`.
    #[must_use]
    pub const fn note_question_answered(question_id: QuestionId) -> NoteQuestionAnswered {
        NoteQuestionAnswered { question_id }
    }

    /// `ProposePlan` with the fixture plan.
    #[must_use]
    pub fn propose_plan() -> ProposePlan {
        ProposePlan::new(plan(), values::usage())
    }

    /// `ApprovePlan` by `valentin`.
    #[must_use]
    pub fn approve_plan() -> ApprovePlan {
        ApprovePlan {
            by: "valentin".to_owned(),
        }
    }

    /// `RejectPlan` with feedback.
    #[must_use]
    pub fn reject_plan() -> RejectPlan {
        RejectPlan {
            by: "valentin".to_owned(),
            feedback: "split the test task".to_owned(),
        }
    }

    /// `StartExecution` with the fixture task ids.
    #[must_use]
    pub fn start_execution() -> StartExecution {
        StartExecution {
            task_ids: task_ids(),
        }
    }

    /// `RecordTaskUsage` for task 1.
    #[must_use]
    pub fn record_task_usage() -> RecordTaskUsage {
        RecordTaskUsage {
            task_id: ids::task_id(1),
            usage: values::usage(),
        }
    }

    /// `NoteTaskTerminal{succeeded}` for `task_id`.
    #[must_use]
    pub fn note_task_succeeded(task_id: TaskId) -> NoteTaskTerminal {
        NoteTaskTerminal {
            task_id,
            outcome: TaskOutcome::Succeeded,
            usage: values::usage(),
        }
    }

    /// `ExhaustBudget` on the wall dimension.
    #[must_use]
    pub fn exhaust_budget() -> ExhaustBudget {
        ExhaustBudget {
            excess: BudgetExcess {
                dimension: BudgetDimension::Wall,
                limit: 7_200_000.into(),
                actual: 7_300_000.into(),
            },
        }
    }

    /// `MarkIntegrated` with one artifact.
    #[must_use]
    pub fn mark_integrated() -> MarkIntegrated {
        MarkIntegrated {
            artifacts: vec![values::artifact()],
            summary: "PR #42 opened".to_owned(),
        }
    }

    /// `MarkEvaluated` with an evaluation.
    #[must_use]
    pub fn mark_evaluated() -> MarkEvaluated {
        MarkEvaluated {
            evaluation: Some(RunEvaluation {
                evaluation_id: ids::evaluation_id(),
                overall: 0.85,
                verdict: Verdict::Accept,
            }),
            summary: "healthz added and tested".to_owned(),
        }
    }

    /// `MarkEvaluated` with the evaluation skipped.
    #[must_use]
    pub fn mark_evaluation_skipped() -> MarkEvaluated {
        MarkEvaluated {
            evaluation: None,
            summary: "healthz added (evaluation skipped)".to_owned(),
        }
    }

    /// `CancelRun`.
    #[must_use]
    pub fn cancel() -> CancelRun {
        CancelRun {
            by: "valentin".to_owned(),
            reason: "changed my mind".to_owned(),
        }
    }

    /// `FailRun{task_failed}`.
    #[must_use]
    pub fn fail() -> FailRun {
        FailRun {
            reason: RunFailureReason::TaskFailed,
            class: FailureClass::Permanent,
            message: Some("t1 failed permanently".to_owned()),
        }
    }

    /// `Evaluate`.
    #[must_use]
    pub fn evaluate() -> Evaluate {
        Evaluate {
            requested_by: "valentin".to_owned(),
        }
    }

    // -- events ------------------------------------------------------------

    /// `run.started` (interactive).
    #[must_use]
    pub fn started() -> RunEvent {
        started_from(&start())
    }

    /// `run.started` (headless).
    #[must_use]
    pub fn started_headless() -> RunEvent {
        started_from(&start_headless())
    }

    /// `run.started` (Kohral).
    #[must_use]
    pub fn started_kohral() -> RunEvent {
        started_from(&start_kohral())
    }

    /// `run.started` from a command.
    #[must_use]
    pub fn started_from(cmd: &StartRun) -> RunEvent {
        RunEvent::Started {
            run_id: cmd.run_id,
            goal: cmd.goal.clone(),
            mode: cmd.mode.clone(),
            budget: cmd.budget.clone(),
            requested_by: cmd.requested_by.clone(),
            auto_approve_plans: cmd.auto_approve_plans,
        }
    }

    /// `run.understanding_started`.
    #[must_use]
    pub fn understanding_started() -> RunEvent {
        RunEvent::UnderstandingStarted {
            planner_route: values::planner_route(),
        }
    }

    /// `run.understanding_completed` without questions.
    #[must_use]
    pub fn understanding_completed() -> RunEvent {
        understanding_completed_with_questions(Vec::new())
    }

    /// `run.understanding_completed` with questions.
    #[must_use]
    pub fn understanding_completed_with_questions(question_ids: Vec<QuestionId>) -> RunEvent {
        RunEvent::UnderstandingCompleted {
            understanding: understanding(),
            usage: values::usage(),
            question_ids,
        }
    }

    /// `run.question_answered`.
    #[must_use]
    pub const fn question_answered(question_id: QuestionId, remaining_open: u32) -> RunEvent {
        RunEvent::QuestionAnswered {
            question_id,
            remaining_open,
        }
    }

    /// `run.plan_proposed` (revision 0).
    #[must_use]
    pub fn plan_proposed() -> RunEvent {
        plan_proposed_revision(0)
    }

    /// `run.plan_proposed` with a revision number.
    #[must_use]
    pub fn plan_proposed_revision(revision: u8) -> RunEvent {
        RunEvent::PlanProposed {
            plan: plan(),
            usage: values::usage(),
            revision,
        }
    }

    /// `run.plan_approved` by `valentin`.
    #[must_use]
    pub fn plan_approved() -> RunEvent {
        RunEvent::PlanApproved {
            by: "valentin".to_owned(),
        }
    }

    /// `run.plan_approved` by `auto`.
    #[must_use]
    pub fn plan_auto_approved() -> RunEvent {
        RunEvent::PlanApproved {
            by: AUTO_APPROVED_BY.to_owned(),
        }
    }

    /// `run.plan_rejected`.
    #[must_use]
    pub fn plan_rejected() -> RunEvent {
        RunEvent::PlanRejected {
            by: "valentin".to_owned(),
            feedback: "split the test task".to_owned(),
        }
    }

    /// `run.execution_started` with the fixture task ids.
    #[must_use]
    pub fn execution_started() -> RunEvent {
        RunEvent::ExecutionStarted {
            task_ids: task_ids(),
        }
    }

    /// `run.usage_recorded` for task 1 (run usage = planner ×2 + task).
    #[must_use]
    pub fn usage_recorded() -> RunEvent {
        RunEvent::UsageRecorded {
            task_id: ids::task_id(1),
            task_usage: values::usage(),
            run_usage: values::usage() + values::usage() + values::usage(),
        }
    }

    /// `run.task_terminal_noted{succeeded}` for `task_id`.
    #[must_use]
    pub fn task_terminal_noted(task_id: TaskId, all_terminal: bool, run_usage: Usage) -> RunEvent {
        RunEvent::TaskTerminalNoted {
            task_id,
            outcome: TaskOutcome::Succeeded,
            task_usage: values::usage(),
            run_usage,
            all_terminal,
        }
    }

    /// `run.budget_exhausted` (wall).
    #[must_use]
    pub fn budget_exhausted() -> RunEvent {
        RunEvent::BudgetExhausted {
            dimension: BudgetDimension::Wall,
            limit: 7_200_000.into(),
            actual: 7_300_000.into(),
        }
    }

    /// `run.integrated`.
    #[must_use]
    pub fn integrated() -> RunEvent {
        RunEvent::Integrated {
            artifacts: vec![values::artifact()],
            summary: "PR #42 opened".to_owned(),
        }
    }

    /// `run.evaluated`.
    #[must_use]
    pub fn evaluated() -> RunEvent {
        RunEvent::Evaluated {
            evaluation_id: ids::evaluation_id(),
            overall: 0.85,
            verdict: Verdict::Accept,
        }
    }

    /// `run.completed`.
    #[must_use]
    pub fn completed(usage: Usage) -> RunEvent {
        RunEvent::Completed {
            summary: "healthz added and tested".to_owned(),
            usage,
            evaluation_skipped: false,
        }
    }

    /// `run.failed{task_failed}`.
    #[must_use]
    pub fn failed(usage: Usage) -> RunEvent {
        RunEvent::Failed {
            reason: RunFailureReason::TaskFailed,
            class: FailureClass::Permanent,
            usage,
            message: Some("t1 failed permanently".to_owned()),
        }
    }

    /// `run.cancelled`.
    #[must_use]
    pub fn cancelled() -> RunEvent {
        RunEvent::Cancelled {
            by: "valentin".to_owned(),
            reason: "changed my mind".to_owned(),
        }
    }

    /// `run.evaluation_requested`.
    #[must_use]
    pub fn evaluation_requested() -> RunEvent {
        RunEvent::EvaluationRequested {
            requested_by: "valentin".to_owned(),
        }
    }

    // -- histories ---------------------------------------------------------

    /// History up to `planning` (interactive, no questions).
    #[must_use]
    pub fn history_planning() -> Vec<RunEvent> {
        vec![
            started(),
            understanding_started(),
            understanding_completed(),
        ]
    }

    /// History up to `awaiting_plan_approval`.
    #[must_use]
    pub fn history_awaiting_approval() -> Vec<RunEvent> {
        let mut h = history_planning();
        h.push(plan_proposed());
        h
    }

    /// History up to `executing` with tasks created.
    #[must_use]
    pub fn history_executing() -> Vec<RunEvent> {
        let mut h = history_awaiting_approval();
        h.push(plan_approved());
        h.push(execution_started());
        h
    }

    /// History up to `integrating` (both tasks succeeded).
    #[must_use]
    pub fn history_integrating() -> Vec<RunEvent> {
        let mut h = history_executing();
        let planner = values::usage() + values::usage();
        h.push(task_terminal_noted(
            ids::task_id(1),
            false,
            planner + values::usage(),
        ));
        h.push(task_terminal_noted(
            ids::task_id(2),
            true,
            planner + values::usage() + values::usage(),
        ));
        h
    }

    /// History up to `evaluating`.
    #[must_use]
    pub fn history_evaluating() -> Vec<RunEvent> {
        let mut h = history_integrating();
        h.push(integrated());
        h
    }

    /// History up to `completed`.
    #[must_use]
    pub fn history_completed() -> Vec<RunEvent> {
        let mut h = history_evaluating();
        let usage = values::usage() + values::usage() + values::usage() + values::usage();
        h.push(evaluated());
        h.push(completed(usage));
        h
    }
}

// ---------------------------------------------------------------------------
// Task fixtures
// ---------------------------------------------------------------------------

/// `Task` command and event builders.
pub mod task {
    use kevin_domain::ids::{AttemptId, QuestionId};
    use kevin_domain::kinds::{FailureClass, TaskKind};
    use kevin_domain::task::{
        CancelTask, CreateTask, FailAttempt, ProvideInput, RecordProgress, RequestInput, RetryTask,
        RouteSelectionInfo, RouteTask, SkipTask, StartAttempt, SucceedAttempt, TaskEvent,
    };
    use kevin_domain::values::Usage;

    use super::ids;
    use super::values;

    // -- commands ----------------------------------------------------------

    /// `CreateTask` (implement, fixture spec, 10 USD / 2 attempts).
    #[must_use]
    pub fn create() -> CreateTask {
        CreateTask {
            task_id: ids::task_id(1),
            run_id: ids::run_id(),
            kind: TaskKind::Implement,
            spec: values::task_spec(),
            budget: values::budget(),
        }
    }

    /// `RouteTask` to the fixture route.
    #[must_use]
    pub fn route() -> RouteTask {
        RouteTask {
            route: values::route(),
            selection: RouteSelectionInfo::fixed(values::route().model),
        }
    }

    /// `RouteTask` to the other route.
    #[must_use]
    pub fn reroute() -> RouteTask {
        RouteTask {
            route: values::other_route(),
            selection: RouteSelectionInfo::fixed(values::other_route().model),
        }
    }

    /// `StartAttempt` with attempt `n`.
    #[must_use]
    pub fn start_attempt(n: u128) -> StartAttempt {
        StartAttempt {
            attempt_id: ids::attempt_id(n),
            workspace: values::workspace(),
            worker_session_id: Some(format!("sess-{n}")),
        }
    }

    /// `RecordProgress` on attempt `n`.
    #[must_use]
    pub fn record_progress(n: u128) -> RecordProgress {
        RecordProgress {
            attempt_id: ids::attempt_id(n),
            summary: "editing src/main.rs".to_owned(),
            usage_delta: values::usage(),
            log_seq: 42,
        }
    }

    /// `RequestInput` on attempt `n`.
    #[must_use]
    pub fn request_input(n: u128, question_id: QuestionId) -> RequestInput {
        RequestInput {
            attempt_id: ids::attempt_id(n),
            question_id,
        }
    }

    /// `ProvideInput` on attempt `n`.
    #[must_use]
    pub fn provide_input(n: u128, question_id: QuestionId) -> ProvideInput {
        ProvideInput {
            attempt_id: ids::attempt_id(n),
            question_id,
        }
    }

    /// `SucceedAttempt` on attempt `n`.
    #[must_use]
    pub fn succeed_attempt(n: u128) -> SucceedAttempt {
        SucceedAttempt {
            attempt_id: ids::attempt_id(n),
            artifacts: vec![values::artifact()],
            usage: values::usage(),
            summary: "done".to_owned(),
        }
    }

    /// `FailAttempt` on attempt `n` with `class`.
    #[must_use]
    pub fn fail_attempt(n: u128, class: FailureClass) -> FailAttempt {
        FailAttempt {
            attempt_id: ids::attempt_id(n),
            class,
            message: format!("{class} failure"),
            usage: values::usage(),
        }
    }

    /// `RetryTask`.
    #[must_use]
    pub fn retry() -> RetryTask {
        RetryTask {
            reason: "transient failure".to_owned(),
        }
    }

    /// `CancelTask`.
    #[must_use]
    pub fn cancel() -> CancelTask {
        CancelTask {
            reason: "run cancelled".to_owned(),
        }
    }

    /// `SkipTask{dependency_failed}`.
    #[must_use]
    pub fn skip() -> SkipTask {
        SkipTask {
            reason: "dependency_failed".to_owned(),
        }
    }

    // -- events ------------------------------------------------------------

    /// `task.created`.
    #[must_use]
    pub fn created() -> TaskEvent {
        let c = create();
        TaskEvent::Created {
            task_id: c.task_id,
            run_id: c.run_id,
            kind: c.kind,
            spec: c.spec,
            budget: c.budget,
        }
    }

    /// `task.routed` to the fixture route.
    #[must_use]
    pub fn routed() -> TaskEvent {
        let r = route();
        TaskEvent::Routed {
            route: r.route,
            selection: r.selection,
        }
    }

    /// `task.routed` to the other route.
    #[must_use]
    pub fn rerouted() -> TaskEvent {
        let r = reroute();
        TaskEvent::Routed {
            route: r.route,
            selection: r.selection,
        }
    }

    /// `task.attempt_started` number `n` (attempt id `n`).
    #[must_use]
    pub fn attempt_started(n: u128) -> TaskEvent {
        TaskEvent::AttemptStarted {
            attempt_id: ids::attempt_id(n),
            attempt_no: u8::try_from(n).unwrap_or(u8::MAX),
            route: values::route(),
            workspace: values::workspace(),
            worker_session_id: Some(format!("sess-{n}")),
        }
    }

    /// `task.progressed` on attempt `n`.
    #[must_use]
    pub fn progressed(n: u128) -> TaskEvent {
        TaskEvent::Progressed {
            attempt_id: ids::attempt_id(n),
            summary: "editing src/main.rs".to_owned(),
            usage_delta: values::usage(),
            log_seq: 42,
        }
    }

    /// `task.input_requested` on attempt `n`.
    #[must_use]
    pub fn input_requested(n: u128, question_id: QuestionId) -> TaskEvent {
        TaskEvent::InputRequested {
            attempt_id: ids::attempt_id(n),
            question_id,
        }
    }

    /// `task.input_provided` on attempt `n`.
    #[must_use]
    pub fn input_provided(n: u128, question_id: QuestionId) -> TaskEvent {
        TaskEvent::InputProvided {
            attempt_id: ids::attempt_id(n),
            question_id,
        }
    }

    /// `task.attempt_succeeded` on attempt `n`.
    #[must_use]
    pub fn attempt_succeeded(n: u128) -> TaskEvent {
        TaskEvent::AttemptSucceeded {
            attempt_id: ids::attempt_id(n),
            artifacts: vec![values::artifact()],
            summary: "done".to_owned(),
            usage: values::usage(),
        }
    }

    /// `task.attempt_failed` on attempt `n`.
    #[must_use]
    pub fn attempt_failed(n: u128, class: FailureClass, retry_possible: bool) -> TaskEvent {
        TaskEvent::AttemptFailed {
            attempt_id: ids::attempt_id(n),
            class,
            message: format!("{class} failure"),
            usage: values::usage(),
            retry_possible,
        }
    }

    /// `task.retried`.
    #[must_use]
    pub fn retried(next_attempt_no: u8) -> TaskEvent {
        TaskEvent::Retried {
            next_attempt_no,
            reason: "transient failure".to_owned(),
        }
    }

    /// `task.cancelled`.
    #[must_use]
    pub fn cancelled() -> TaskEvent {
        TaskEvent::Cancelled {
            reason: "run cancelled".to_owned(),
        }
    }

    /// `task.cancelled` carrying a failed attempt's message.
    #[must_use]
    pub fn cancelled_with(reason: impl Into<String>) -> TaskEvent {
        TaskEvent::Cancelled {
            reason: reason.into(),
        }
    }

    /// `task.skipped`.
    #[must_use]
    pub fn skipped() -> TaskEvent {
        TaskEvent::Skipped {
            reason: "dependency_failed".to_owned(),
        }
    }

    /// Attempt id `n`.
    #[must_use]
    pub const fn attempt(n: u128) -> AttemptId {
        ids::attempt_id(n)
    }

    /// Zero usage (for "worker reported nothing" cases).
    #[must_use]
    pub const fn no_usage() -> Usage {
        Usage::ZERO
    }

    // -- histories ---------------------------------------------------------

    /// `created, routed`.
    #[must_use]
    pub fn history_routed() -> Vec<TaskEvent> {
        vec![created(), routed()]
    }

    /// `created, routed, attempt_started(1)`.
    #[must_use]
    pub fn history_running() -> Vec<TaskEvent> {
        vec![created(), routed(), attempt_started(1)]
    }

    /// `… attempt_failed(1, transient, retry_possible)`.
    #[must_use]
    pub fn history_failed_retryable() -> Vec<TaskEvent> {
        let mut h = history_running();
        h.push(attempt_failed(1, FailureClass::Transient, true));
        h
    }
}

// ---------------------------------------------------------------------------
// Question fixtures
// ---------------------------------------------------------------------------

/// `Question` command and event builders.
pub mod question {
    use std::time::Duration;

    use kevin_domain::question::{AnswerQuestion, AskQuestion, ExpireQuestion, QuestionEvent};
    use kevin_domain::values::{Answer, QuestionOption, QuestionPolicy};

    use super::ids;

    /// Options `yes` (recommended) / `no`.
    #[must_use]
    pub fn options() -> Vec<QuestionOption> {
        vec![
            QuestionOption::new("yes").recommended(),
            QuestionOption::new("no"),
        ]
    }

    /// `AskQuestion` (blocking, yes/no, no default).
    #[must_use]
    pub fn ask() -> AskQuestion {
        AskQuestion {
            question_id: ids::question_id(1),
            run_id: ids::run_id(),
            task_id: None,
            text: "Should /healthz check the database?".to_owned(),
            options: options(),
            multi_select: false,
            default: None,
            policy: QuestionPolicy::Block,
        }
    }

    /// `AskQuestion` headless with a default (`yes`) after 10 minutes.
    #[must_use]
    pub fn ask_with_default() -> AskQuestion {
        AskQuestion {
            default: Some(Answer::selected(["yes"], Answer::DEFAULT_ANSWERED_BY)),
            policy: QuestionPolicy::DefaultAfter {
                timeout: Duration::from_secs(600),
            },
            ..ask()
        }
    }

    /// `AskQuestion` headless without a default.
    #[must_use]
    pub fn ask_without_default() -> AskQuestion {
        AskQuestion {
            default: None,
            policy: QuestionPolicy::DefaultAfter {
                timeout: Duration::from_secs(600),
            },
            ..ask()
        }
    }

    /// `AnswerQuestion{selected: [no]}` by `valentin`.
    #[must_use]
    pub fn answer() -> AnswerQuestion {
        AnswerQuestion {
            answer: Answer::selected(["no"], "valentin"),
        }
    }

    /// `ExpireQuestion`.
    #[must_use]
    pub const fn expire() -> ExpireQuestion {
        ExpireQuestion
    }

    /// `question.asked` from a command.
    #[must_use]
    pub fn asked_from(cmd: &AskQuestion) -> QuestionEvent {
        QuestionEvent::Asked {
            question_id: cmd.question_id,
            run_id: cmd.run_id,
            task_id: cmd.task_id,
            text: cmd.text.clone(),
            options: cmd.options.clone(),
            multi_select: cmd.multi_select,
            default: cmd.default.clone(),
            policy: cmd.policy,
        }
    }

    /// `question.asked` (blocking).
    #[must_use]
    pub fn asked() -> QuestionEvent {
        asked_from(&ask())
    }

    /// `question.asked` (headless with default).
    #[must_use]
    pub fn asked_with_default() -> QuestionEvent {
        asked_from(&ask_with_default())
    }

    /// `question.asked` (headless without default).
    #[must_use]
    pub fn asked_without_default() -> QuestionEvent {
        asked_from(&ask_without_default())
    }

    /// `question.answered{no, valentin}`.
    #[must_use]
    pub fn answered() -> QuestionEvent {
        QuestionEvent::Answered {
            answer: Answer::selected(["no"], "valentin"),
            answered_by: "valentin".to_owned(),
        }
    }

    /// `question.answered{yes, default}`.
    #[must_use]
    pub fn answered_by_default() -> QuestionEvent {
        QuestionEvent::Answered {
            answer: Answer::selected(["yes"], Answer::DEFAULT_ANSWERED_BY),
            answered_by: Answer::DEFAULT_ANSWERED_BY.to_owned(),
        }
    }

    /// `question.expired`.
    #[must_use]
    pub const fn expired(applied_default: bool) -> QuestionEvent {
        QuestionEvent::Expired { applied_default }
    }
}

// ---------------------------------------------------------------------------
// Evaluation fixtures
// ---------------------------------------------------------------------------

/// `Evaluation` command and event builders.
pub mod evaluation {
    use kevin_domain::evaluation::{
        AcceptProposal, EvaluationEvent, ProposalDraft, RecordEvaluation, RejectProposal,
    };
    use kevin_domain::values::{EvaluationSubject, Proposal, ProposalKind, Verdict};

    use super::ids;
    use super::values;

    /// One routing proposal draft.
    #[must_use]
    pub fn proposal_draft() -> ProposalDraft {
        ProposalDraft {
            id: ids::proposal_id(1),
            kind: ProposalKind::Routing,
            body: "prefer gpt56-codex for test tasks".to_owned(),
            rationale: "faster and cheaper on this repo".to_owned(),
        }
    }

    /// `RecordEvaluation` for the fixture run.
    #[must_use]
    pub fn record() -> RecordEvaluation {
        RecordEvaluation {
            evaluation_id: ids::evaluation_id(),
            subject: EvaluationSubject::Run(ids::run_id()),
            rubric_id: "default".to_owned(),
            judge_route: values::planner_route(),
            scores: values::scores(),
            overall: 0.85,
            verdict: Verdict::Accept,
            lessons: vec!["Add a health check test alongside the route.".to_owned()],
            proposals: vec![proposal_draft()],
            usage: values::usage(),
        }
    }

    /// `AcceptProposal` for proposal 1.
    #[must_use]
    pub fn accept_proposal() -> AcceptProposal {
        AcceptProposal {
            proposal_id: ids::proposal_id(1),
            by: "valentin".to_owned(),
            note: Some("routing looks right".to_owned()),
        }
    }

    /// `RejectProposal` for proposal 1.
    #[must_use]
    pub fn reject_proposal() -> RejectProposal {
        RejectProposal {
            proposal_id: ids::proposal_id(1),
            by: "valentin".to_owned(),
            note: Some("we already tried this alias".to_owned()),
        }
    }

    /// `evaluation.recorded`.
    #[must_use]
    pub fn recorded() -> EvaluationEvent {
        let c = record();
        EvaluationEvent::Recorded {
            evaluation_id: c.evaluation_id,
            subject: c.subject,
            rubric_id: c.rubric_id,
            judge_route: c.judge_route,
            scores: c.scores,
            overall: c.overall,
            verdict: c.verdict,
            lessons: c.lessons,
            proposals: c.proposals.into_iter().map(Proposal::from).collect(),
            usage: c.usage,
        }
    }

    /// `evaluation.proposal_accepted`.
    #[must_use]
    pub fn proposal_accepted() -> EvaluationEvent {
        EvaluationEvent::ProposalAccepted {
            proposal_id: ids::proposal_id(1),
            by: "valentin".to_owned(),
            note: Some("routing looks right".to_owned()),
        }
    }

    /// `evaluation.proposal_rejected`.
    #[must_use]
    pub fn proposal_rejected() -> EvaluationEvent {
        EvaluationEvent::ProposalRejected {
            proposal_id: ids::proposal_id(1),
            by: "valentin".to_owned(),
            note: Some("we already tried this alias".to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// RouteScore fixtures
// ---------------------------------------------------------------------------

/// `RouteScore` command and event builders.
pub mod route_score {
    use kevin_domain::kinds::{FailureClass, ModelAlias, TaskKind, Tier};
    use kevin_domain::route_score::{
        BetaPrior, RecordRouteOutcome, ResetRouteScore, RouteScoreEvent, RouteStats,
    };

    use super::fixture_time;
    use super::values::rust_decimal_shim::usd;

    /// `sonnet5-claude`.
    #[must_use]
    pub fn alias() -> ModelAlias {
        ModelAlias::new("sonnet5-claude").unwrap_or_else(|e| panic!("{e}"))
    }

    /// A successful `implement` outcome with quality 0.8.
    #[must_use]
    pub fn success() -> RecordRouteOutcome {
        RecordRouteOutcome {
            task_kind: TaskKind::Implement,
            alias: alias(),
            success: true,
            quality: Some(0.8),
            cost_usd: Some(usd("0.40")),
            wall_ms: 60_000,
            failure_class: None,
            recorded_at: fixture_time(),
            prior: BetaPrior::for_tier(Tier::Balanced),
        }
    }

    /// A permanent failure outcome.
    #[must_use]
    pub fn permanent_failure() -> RecordRouteOutcome {
        RecordRouteOutcome {
            success: false,
            quality: None,
            cost_usd: Some(usd("0.10")),
            failure_class: Some(FailureClass::Permanent),
            ..success()
        }
    }

    /// A transient failure outcome (does not blame the model).
    #[must_use]
    pub fn transient_failure() -> RecordRouteOutcome {
        RecordRouteOutcome {
            failure_class: Some(FailureClass::Transient),
            ..permanent_failure()
        }
    }

    /// `ResetRouteScore` to the balanced prior.
    #[must_use]
    pub fn reset() -> ResetRouteScore {
        ResetRouteScore {
            task_kind: TaskKind::Implement,
            alias: alias(),
            prior: BetaPrior::for_tier(Tier::Balanced),
        }
    }

    /// `routing.score_updated` after one success from the balanced prior.
    #[must_use]
    pub fn score_updated_after_success() -> RouteScoreEvent {
        let mut stats = RouteStats::from_prior(BetaPrior::for_tier(Tier::Balanced));
        stats.attempts = 1;
        stats.successes = 1;
        stats.sum_quality = 0.8;
        stats.quality_samples = 1;
        stats.sum_cost_usd = usd("0.40");
        stats.cost_samples = 1;
        stats.sum_wall_ms = 60_000;
        stats.alpha += 1.0;
        stats.quality_ema = Some(0.8);
        stats.last_used = Some(fixture_time());
        RouteScoreEvent::ScoreUpdated {
            task_kind: TaskKind::Implement,
            alias: alias(),
            stats,
            success: Some(true),
            reset: false,
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryItem fixtures
// ---------------------------------------------------------------------------

/// `MemoryItem` command and event builders.
pub mod memory_item {
    use kevin_domain::envelope::Actor;
    use kevin_domain::memory_item::{
        ForgetMemoryItem, MemoryItemEvent, StoreMemoryItem, SupersedeMemoryItem,
    };
    use kevin_domain::values::{MemoryKind, MemoryScope, MemorySource};

    use super::fixture_time;
    use super::ids;

    /// `StoreMemoryItem` (lesson from the fixture evaluation).
    #[must_use]
    pub fn store() -> StoreMemoryItem {
        StoreMemoryItem {
            memory_item_id: ids::memory_item_id(1),
            kind: MemoryKind::Lesson,
            content: "Add a health check test alongside the route.".to_owned(),
            tags: vec!["implement".to_owned(), "repo:abc".to_owned()],
            source: MemorySource {
                run_id: Some(ids::run_id()),
                task_id: None,
                evaluation_id: Some(ids::evaluation_id()),
                actor: Actor::system("evaluator"),
            },
            scope: MemoryScope::Repo("abc".to_owned()),
            embedding_model: Some("BAAI/bge-small-en-v1.5".to_owned()),
            importance: 0.5,
            created_at: fixture_time(),
        }
    }

    /// `SupersedeMemoryItem` by item 2.
    #[must_use]
    pub fn supersede() -> SupersedeMemoryItem {
        SupersedeMemoryItem {
            superseded_by: ids::memory_item_id(2),
        }
    }

    /// `ForgetMemoryItem`.
    #[must_use]
    pub fn forget() -> ForgetMemoryItem {
        ForgetMemoryItem {
            reason: "contains a secret".to_owned(),
        }
    }

    /// `memory.item_stored`.
    #[must_use]
    pub fn stored() -> MemoryItemEvent {
        let c = store();
        MemoryItemEvent::Stored {
            memory_item_id: c.memory_item_id,
            kind: c.kind,
            content: c.content,
            tags: c.tags,
            source: c.source,
            scope: c.scope,
            embedding_model: c.embedding_model,
            importance: c.importance,
            created_at: c.created_at,
        }
    }

    /// `memory.item_superseded`.
    #[must_use]
    pub fn superseded() -> MemoryItemEvent {
        MemoryItemEvent::Superseded {
            superseded_by: ids::memory_item_id(2),
        }
    }

    /// `memory.item_forgotten`.
    #[must_use]
    pub fn forgotten() -> MemoryItemEvent {
        MemoryItemEvent::Forgotten {
            reason: "contains a secret".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Invariant checkers
// ---------------------------------------------------------------------------

/// Panics if `run` violates a `Run` invariant (`plan/02-domain-model.md`):
/// executing requires an approved plan; open questions only while awaiting
/// answers; a crossed USD/token budget is always recorded; terminal statuses
/// carry their failure reason.
#[track_caller]
pub fn assert_run_invariants(run: &kevin_domain::run::Run) {
    use kevin_domain::run::RunStatus;

    let status = run.status();
    if matches!(
        status,
        RunStatus::Executing | RunStatus::Integrating | RunStatus::Evaluating
    ) {
        assert!(
            run.plan_approved(),
            "run is {status} without an approved plan"
        );
        assert!(run.plan().is_some(), "run is {status} without a plan");
    }
    if status == RunStatus::AwaitingAnswers {
        assert!(
            !run.open_question_ids().is_empty(),
            "awaiting_answers with no open questions"
        );
    } else {
        assert!(
            run.open_question_ids().is_empty(),
            "run in {status} still has open questions {:?}",
            run.open_question_ids()
        );
    }
    if !run.task_ids().is_empty() {
        assert!(run.plan_approved(), "tasks exist without an approved plan");
    }
    if let Some(excess) = run.budget().exceeded_by(run.usage()) {
        assert!(
            run.budget_exhausted().is_some(),
            "usage {:?} exceeds budget ({excess:?}) but budget_exhausted is not recorded",
            run.usage()
        );
    }
    if status == RunStatus::Failed {
        assert!(run.failure().is_some(), "failed run without a reason");
    }
    let rolled: kevin_domain::values::Usage = run.task_usage().values().sum();
    assert!(
        run.usage().total_tokens() >= rolled.total_tokens(),
        "run usage smaller than the sum of task usage"
    );
}

/// Panics if `task` violates a `Task` invariant: at most one active attempt;
/// attempts ≤ `max_attempts`; attempt numbers are 1..n; status ↔ attempt
/// state agree; `StartAttempt` only ever happened with a route; usage is the
/// sum of attempt usage.
#[track_caller]
pub fn assert_task_invariants(task: &kevin_domain::task::Task) {
    use kevin_domain::task::{AttemptStatus, TaskStatus};

    let active = task.attempts().iter().filter(|a| a.is_active()).count();
    assert!(active <= 1, "task has {active} active attempts");
    assert!(
        task.attempts().len() <= usize::from(task.budget().max_attempts),
        "task has {} attempts, max is {}",
        task.attempts().len(),
        task.budget().max_attempts
    );
    for (i, attempt) in task.attempts().iter().enumerate() {
        assert_eq!(
            usize::from(attempt.no),
            i + 1,
            "attempt numbers must be 1..n"
        );
    }
    if !task.attempts().is_empty() {
        assert!(task.route().is_some(), "attempts exist without a route");
    }
    match task.status() {
        TaskStatus::Running => assert!(
            task.active_attempt()
                .is_some_and(|a| a.status == AttemptStatus::Running),
            "status running without a running attempt"
        ),
        TaskStatus::AwaitingInput => assert!(
            task.active_attempt().is_some_and(
                |a| a.status == AttemptStatus::AwaitingInput && a.pending_question.is_some()
            ),
            "status awaiting_input without a paused attempt"
        ),
        TaskStatus::Succeeded => assert!(
            task.last_attempt()
                .is_some_and(|a| a.status == AttemptStatus::Succeeded),
            "status succeeded without a succeeded attempt"
        ),
        TaskStatus::Failed => assert!(
            task.last_attempt()
                .is_some_and(|a| a.status == AttemptStatus::Failed),
            "status failed without a failed attempt"
        ),
        TaskStatus::Pending | TaskStatus::Routed | TaskStatus::Cancelled | TaskStatus::Skipped => {
            assert_eq!(active, 0, "status {} with an active attempt", task.status());
        }
    }
    let summed: kevin_domain::values::Usage = task.attempts().iter().map(|a| a.usage).sum();
    assert_eq!(
        *task.usage(),
        summed,
        "task usage must equal the sum of attempt usage"
    );
}
