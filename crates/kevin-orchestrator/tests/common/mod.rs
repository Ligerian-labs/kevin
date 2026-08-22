//! Harness for the WS-08 acceptance scenarios (`plan/05-orchestration.md` §6).
//!
//! Every scenario boots a real orchestrator on a per-test Postgres database
//! (`kevin_testkit::pg::TestDb`) with the in-process `fake` worker and the
//! scripted ports from `kevin_orchestrator::testing`. No CLI is ever invoked
//! and no repository is touched: workspaces are plain temp directories.
//!
//! Assertions are on the **event stream of the run** read back from
//! `core.events`, which is the contract `plan/05` §6 pins down.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use kevin_bus::InProcBus;
use kevin_config::{
    Integration, KevinConfig, ModelEntry, Role as ConfigRole, WorkspaceCleanup, WorkspaceStrategy,
};
use kevin_domain::question::AnswerQuestion;
use kevin_domain::run::{ApprovePlan, CancelRun, RejectPlan, StartRun};
use kevin_domain::{
    Actor, Answer, Budget, Complexity, Goal, IdGen, ModelAlias, Plan, PlanTask, ProposedQuestion,
    QuestionId, QuestionOption, RunId, RunMode, Understanding, UuidV7IdGen, WorkerKind,
};
use kevin_orchestrator::orchestrator::{Deps, Handle, Orchestrator};
use kevin_orchestrator::ports::{RolesPort, RouterPort};
use kevin_orchestrator::projections::TaskLog;
use kevin_orchestrator::services::CommandContext;
use kevin_orchestrator::testing::{
    FixedRouter, RecordingMemory, ScriptedEvaluator, ScriptedRoles, TempWorkspaces, fake_alt_route,
    fake_route,
};
use kevin_store::{CommandLog, EventStore, PgEventStore, StoredEvent};
use kevin_testkit::pg::TestDb;
use kevin_worker::fake::{FakeWorker, Scenario};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::{SandboxPolicy, Worker};
use tempfile::TempDir;

/// How long a scenario waits for the event it expects.
///
/// These scenarios drive a real engine over a real Postgres, so the deadline
/// has to survive a loaded machine: nextest runs the whole workspace in
/// parallel and several agents share one database server. 20 s was enough on
/// an idle laptop and produced false failures everywhere else. Raise it
/// further with `KEVIN_TEST_WAIT_SECS` on very slow hardware.
///
/// A deadline only decides *when to give up*; no assertion depends on it, so
/// a longer one cannot hide a bug — it only stops the suite reporting one that
/// is not there.
pub fn wait_timeout() -> Duration {
    std::env::var("KEVIN_TEST_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(DEFAULT_WAIT, Duration::from_secs)
}

/// Default of [`wait_timeout`].
const DEFAULT_WAIT: Duration = Duration::from_secs(90);
/// Polling interval of [`Harness::wait_until`].
const POLL: Duration = Duration::from_millis(15);

/// The two `fake` aliases every scenario configures.
pub const ALIAS: &str = "fake";
/// Second alias, so a retry can reroute.
pub const ALIAS_ALT: &str = "fake-alt";

/// What a scenario wires before booting.
pub struct Setup {
    /// Fake-worker script.
    pub scenario: Scenario,
    /// Worker override (e.g. `FlakyWorker`); built from `scenario` when `None`.
    pub worker: Option<Arc<dyn Worker>>,
    /// Planner / integrator answers.
    pub roles: Arc<ScriptedRoles>,
    /// Route selection.
    pub router: Arc<FixedRouter>,
    /// Workspaces and integration.
    pub workspaces: Option<Arc<TempWorkspaces>>,
    /// The judge; `None` completes runs with `evaluation: skipped`.
    pub evaluator: Option<Arc<ScriptedEvaluator>>,
    /// Long-term memory.
    pub memory: Option<Arc<RecordingMemory>>,
    /// Applied to the default test configuration.
    pub config: Box<dyn FnOnce(&mut KevinConfig) + Send>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            scenario: Scenario::replying("done"),
            worker: None,
            roles: Arc::new(ScriptedRoles::new()),
            router: Arc::new(FixedRouter::new(vec![fake_route(), fake_alt_route()])),
            workspaces: None,
            evaluator: None,
            memory: None,
            config: Box::new(|_| {}),
        }
    }
}

impl Setup {
    /// A setup replying `done` to every prompt.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the fake-worker script.
    #[must_use]
    pub fn scenario(mut self, scenario: Scenario) -> Self {
        self.scenario = scenario;
        self
    }

    /// Sets the planner/integrator script.
    #[must_use]
    pub fn roles(mut self, roles: Arc<ScriptedRoles>) -> Self {
        self.roles = roles;
        self
    }

    /// Sets the judge.
    #[must_use]
    pub fn evaluator(mut self, evaluator: Arc<ScriptedEvaluator>) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    /// Sets the memory port.
    #[must_use]
    pub fn memory(mut self, memory: Arc<RecordingMemory>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Sets the workspace port.
    #[must_use]
    pub fn workspaces(mut self, workspaces: Arc<TempWorkspaces>) -> Self {
        self.workspaces = Some(workspaces);
        self
    }

    /// Overrides the worker adapter.
    #[must_use]
    pub fn worker(mut self, worker: Arc<dyn Worker>) -> Self {
        self.worker = Some(worker);
        self
    }

    /// Tweaks the configuration.
    #[must_use]
    pub fn config(mut self, f: impl FnOnce(&mut KevinConfig) + Send + 'static) -> Self {
        self.config = Box::new(f);
        self
    }
}

/// A booted orchestrator with everything the scenario may assert on.
pub struct Harness {
    /// Per-test database (dropped with the harness).
    pub db: TestDb,
    /// Temp root for workspaces, transcripts and the data dir.
    pub tmp: TempDir,
    /// The event store.
    pub store: Arc<PgEventStore>,
    /// The in-process bus.
    pub bus: Arc<InProcBus>,
    /// Effective configuration.
    pub config: Arc<KevinConfig>,
    /// Scripted roles.
    pub roles: Arc<ScriptedRoles>,
    /// Route selection.
    pub router: Arc<FixedRouter>,
    /// Workspaces and integration.
    pub workspaces: Arc<TempWorkspaces>,
    /// The judge, when one was wired.
    pub evaluator: Option<Arc<ScriptedEvaluator>>,
    /// Memory, when one was wired.
    pub memory: Option<Arc<RecordingMemory>>,
    /// Id generator.
    pub ids: Arc<UuidV7IdGen>,
    /// Command idempotency log.
    pub commands: Arc<CommandLog>,
    /// Worker registry.
    pub workers: Arc<WorkerRegistry>,
    /// `orch.task_log` writer.
    pub task_log: Arc<TaskLog>,
    /// Saga tick interval.
    pub tick: Duration,
    /// Per-run `[roles]` overrides every [`Harness::start`] passes.
    pub role_overrides: kevin_domain::RoleOverrides,
    /// Everything needed to build a fresh [`Deps`] (reboot).
    ports: Ports,
    /// The live engine.
    pub handle: Handle,
}

/// The shared components a [`Deps`] is built from, so a scenario can reboot
/// the engine over the same store (`runtime_restarted_on_boot`).
#[derive(Clone)]
struct Ports {
    store: Arc<PgEventStore>,
    bus: Arc<InProcBus>,
    commands: Arc<CommandLog>,
    workers: Arc<WorkerRegistry>,
    workspaces: Arc<TempWorkspaces>,
    router: Arc<FixedRouter>,
    roles: Arc<ScriptedRoles>,
    evaluator: Option<Arc<ScriptedEvaluator>>,
    memory: Option<Arc<RecordingMemory>>,
    config: Arc<KevinConfig>,
    ids: Arc<UuidV7IdGen>,
    task_log: Arc<TaskLog>,
    tick: Duration,
}

impl Ports {
    fn deps(&self) -> Deps {
        Deps {
            store: self.store.clone(),
            bus: self.bus.clone(),
            commands: self.commands.clone(),
            workers: self.workers.clone(),
            workspace: self.workspaces.clone(),
            router: self.router.clone() as Arc<dyn RouterPort>,
            roles: self.roles.clone() as Arc<dyn RolesPort>,
            memory: self
                .memory
                .clone()
                .map(|m| m as Arc<dyn kevin_orchestrator::ports::MemoryPort>),
            evaluator: self
                .evaluator
                .clone()
                .map(|e| e as Arc<dyn kevin_orchestrator::ports::EvaluatorPort>),
            config: self.config.clone(),
            clock: Arc::new(kevin_domain::SystemClock),
            ids: self.ids.clone(),
            system_context: Vec::new(),
            task_log: Some(self.task_log.clone()),
            tick_interval: self.tick,
        }
    }
}

/// The default test configuration: only the `fake` worker, tiny timings.
pub fn test_config(data_dir: &std::path::Path) -> KevinConfig {
    let mut config = KevinConfig::default();
    config.kevin.data_dir = data_dir.to_path_buf();
    config.kevin.shutdown_grace_period = Duration::from_millis(300);
    config.kevin.auto_approve_plans = false;

    config.models.clear();
    for alias in [ALIAS, ALIAS_ALT] {
        config.models.insert(
            ModelAlias::new(alias).expect("valid alias"),
            ModelEntry::new(WorkerKind::Fake, alias),
        );
    }
    let fake = ModelAlias::new(ALIAS).expect("valid alias");
    config.roles.planner = fake.clone();
    config.roles.clarifier = fake.clone();
    config.roles.judge = fake.clone();
    config.roles.integrator = fake.clone();
    config.roles.default = fake;
    config.roles.effort.clear();
    let _ = ConfigRole::ALL;

    config.budget.default_run_wall = Duration::from_secs(120);
    config.budget.default_task_wall = Duration::from_secs(15);
    config.budget.default_task_usd = rust_decimal::Decimal::new(100, 0);
    config.budget.max_parallel_tasks = 8;
    config.budget.max_attempts = 2;

    config.orchestrator.progress_interval = Duration::from_millis(5);
    config.orchestrator.question_default_timeout = Duration::from_millis(120);
    config.orchestrator.evaluation_timeout = Duration::from_secs(10);
    config.orchestrator.role_call_timeout = Duration::from_secs(10);

    config.workspace.strategy = WorkspaceStrategy::InPlace;
    config.workspace.cleanup = WorkspaceCleanup::Never;
    config.workspace.integration = Integration::Pr;

    config.concurrency.per_worker_kind.clear();
    config
        .concurrency
        .per_worker_kind
        .insert(WorkerKind::Fake, 64);
    config
}

impl Harness {
    /// Boots the engine for `setup`.
    pub async fn boot(setup: Setup) -> Self {
        let db = TestDb::new().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut config = test_config(tmp.path());
        (setup.config)(&mut config);
        let config = Arc::new(config);

        let store = Arc::new(PgEventStore::new(db.pool().clone()));
        let bus = Arc::new(InProcBus::with_defaults());
        let commands = Arc::new(CommandLog::new(db.pool().clone()));
        let task_log = Arc::new(TaskLog::new(db.pool().clone()));

        let mut registry_cfg = RegistryConfig::from(&*config);
        registry_cfg.data_dir = tmp.path().join("transcripts");
        let mut registry = WorkerRegistry::empty(registry_cfg, SandboxPolicy::cli_native());
        let worker = setup.worker.unwrap_or_else(|| {
            Arc::new(FakeWorker::new(
                setup.scenario,
                tmp.path().join("transcripts"),
            ))
        });
        registry.insert(worker);

        let workspaces = setup
            .workspaces
            .unwrap_or_else(|| Arc::new(TempWorkspaces::new(tmp.path().join("workspaces"))));
        let ids = Arc::new(UuidV7IdGen);
        let workers = Arc::new(registry);
        let tick = Duration::from_millis(40);

        let ports = Ports {
            store: store.clone(),
            bus: bus.clone(),
            commands: commands.clone(),
            workers: workers.clone(),
            workspaces: workspaces.clone(),
            router: setup.router.clone(),
            roles: setup.roles.clone(),
            evaluator: setup.evaluator.clone(),
            memory: setup.memory.clone(),
            config: config.clone(),
            ids: ids.clone(),
            task_log: task_log.clone(),
            tick,
        };
        let handle = Orchestrator::boot(ports.deps()).await.expect("boot");

        Self {
            db,
            tmp,
            store,
            bus,
            config,
            roles: setup.roles,
            router: setup.router,
            workspaces,
            evaluator: setup.evaluator,
            memory: setup.memory,
            ids,
            commands,
            workers,
            task_log,
            tick,
            role_overrides: kevin_domain::RoleOverrides::new(),
            ports,
            handle,
        }
    }

    /// Kills the engine without recording anything (crash simulation).
    ///
    /// Waits for the aborted actor tasks to have really stopped instead of
    /// sleeping: a fixed pause races the reboot on a loaded machine, and the
    /// "dead" engine appending one more event after the new one booted is
    /// exactly the corruption these scenarios are meant to rule out.
    pub async fn crash(&self) {
        self.handle.abort_and_join().await;
    }

    /// Boots a fresh engine over the same store — the restart path.
    pub async fn reboot(&mut self) {
        self.handle = Orchestrator::boot(self.ports.deps()).await.expect("reboot");
    }

    /// A system command context for `run_id`.
    pub fn ctx(&self, run_id: RunId) -> CommandContext {
        CommandContext::user(self.ids.as_ref(), run_id, "tester")
    }

    /// Starts a run with the default budget.
    pub async fn start(&self, goal: &str, mode: RunMode) -> RunId {
        self.start_with(goal, mode, default_budget()).await
    }

    /// Starts a run with an explicit budget.
    pub async fn start_with(&self, goal: &str, mode: RunMode, budget: Budget) -> RunId {
        let run_id = self.ids.run_id();
        let ctx = self.ctx(run_id);
        self.handle
            .start_run(
                StartRun {
                    run_id,
                    goal: Goal::new(goal, self.tmp.path()),
                    mode,
                    budget,
                    requested_by: "tester".to_owned(),
                    auto_approve_plans: false,
                    role_overrides: self.role_overrides.clone(),
                },
                &ctx,
            )
            .await
            .expect("start run")
    }

    /// Every event of `run_id`, in global order.
    pub async fn events(&self, run_id: RunId) -> Vec<StoredEvent> {
        let mut out = Vec::new();
        let mut position = 0;
        loop {
            let page = self.store.read_all(position, 500).await.expect("read_all");
            if page.is_empty() {
                break;
            }
            position = page.last().map_or(position, |e| e.position);
            out.extend(
                page.into_iter()
                    .filter(|e| e.envelope.correlation_id == run_id.as_uuid()),
            );
        }
        out
    }

    /// `event_type`s of `run_id`, in global order.
    pub async fn types(&self, run_id: RunId) -> Vec<&'static str> {
        self.events(run_id)
            .await
            .iter()
            .map(|e| e.envelope.event_type)
            .collect()
    }

    /// Waits until `pred` holds over the run's events; panics on timeout.
    pub async fn wait_until(
        &self,
        run_id: RunId,
        label: &str,
        pred: impl Fn(&[StoredEvent]) -> bool,
    ) -> Vec<StoredEvent> {
        let deadline = tokio::time::Instant::now() + wait_timeout();
        let mut last = Vec::new();
        while tokio::time::Instant::now() < deadline {
            last = self.events(run_id).await;
            if pred(&last) {
                return last;
            }
            tokio::time::sleep(POLL).await;
        }
        let seen: Vec<&str> = last.iter().map(|e| e.envelope.event_type).collect();
        panic!("timed out waiting for {label}; events so far: {seen:?}");
    }

    /// Waits for one occurrence of `event_type`.
    pub async fn wait_for(&self, run_id: RunId, event_type: &str) -> Vec<StoredEvent> {
        self.wait_until(run_id, event_type, |events| {
            events.iter().any(|e| e.envelope.event_type == event_type)
        })
        .await
    }

    /// Waits for `n` occurrences of `event_type`.
    pub async fn wait_for_n(&self, run_id: RunId, event_type: &str, n: usize) -> Vec<StoredEvent> {
        self.wait_until(run_id, &format!("{n}× {event_type}"), |events| {
            count(events, event_type) >= n
        })
        .await
    }

    /// Waits until the run reaches a terminal event.
    pub async fn wait_terminal(&self, run_id: RunId) -> Vec<StoredEvent> {
        self.wait_until(run_id, "a terminal run event", |events| {
            events.iter().any(|e| {
                matches!(
                    e.envelope.event_type,
                    "run.completed" | "run.failed" | "run.cancelled"
                )
            })
        })
        .await
    }

    /// The ids of the questions asked by the run, in ask order.
    pub async fn questions(&self, run_id: RunId) -> Vec<QuestionId> {
        self.events(run_id)
            .await
            .iter()
            .filter(|e| e.envelope.event_type == "question.asked")
            .map(|e| QuestionId::from_uuid(e.envelope.aggregate_id))
            .collect()
    }

    /// Answers `question_id` by selecting `label`.
    pub async fn answer(&self, run_id: RunId, question_id: QuestionId, label: &str) {
        let ctx = self.ctx(run_id);
        self.handle
            .question_service()
            .answer(
                question_id,
                AnswerQuestion {
                    answer: Answer::selected([label.to_owned()], "tester"),
                },
                &ctx,
            )
            .await
            .expect("answer question");
    }

    /// Approves the proposed plan.
    pub async fn approve(&self, run_id: RunId) {
        let ctx = self.ctx(run_id);
        self.handle
            .run_service()
            .approve_plan(
                run_id,
                ApprovePlan {
                    by: "tester".to_owned(),
                },
                &ctx,
            )
            .await
            .expect("approve plan");
    }

    /// Rejects the proposed plan with `feedback`.
    pub async fn reject(&self, run_id: RunId, feedback: &str) {
        let ctx = self.ctx(run_id);
        self.handle
            .run_service()
            .reject_plan(
                run_id,
                RejectPlan {
                    by: "tester".to_owned(),
                    feedback: feedback.to_owned(),
                },
                &ctx,
            )
            .await
            .expect("reject plan");
    }

    /// Cancels the run.
    pub async fn cancel(&self, run_id: RunId, reason: &str) {
        let ctx = self.ctx(run_id);
        self.handle
            .run_service()
            .cancel(
                run_id,
                CancelRun {
                    by: "tester".to_owned(),
                    reason: reason.to_owned(),
                },
                &ctx,
            )
            .await
            .expect("cancel run");
    }

    /// Payload of the first `event_type` event of the run.
    pub async fn payload(&self, run_id: RunId, event_type: &str) -> serde_json::Value {
        self.events(run_id)
            .await
            .into_iter()
            .find(|e| e.envelope.event_type == event_type)
            .unwrap_or_else(|| panic!("no `{event_type}` event"))
            .envelope
            .payload
    }

    /// Drains and shuts the engine down.
    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}

/// The budget scenarios start with unless they say otherwise.
pub fn default_budget() -> Budget {
    Budget {
        max_usd: Some(rust_decimal::Decimal::new(100, 0)),
        max_tokens: None,
        max_wall: Some(Duration::from_secs(120)),
        max_attempts: 2,
        max_parallel: 4,
    }
}

/// How often `event_type` appears.
pub fn count(events: &[StoredEvent], event_type: &str) -> usize {
    events
        .iter()
        .filter(|e| e.envelope.event_type == event_type)
        .count()
}

/// Polls `check` until it holds; panics after [`wait_timeout`].
pub async fn eventually(label: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + wait_timeout();
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
    panic!("timed out waiting for {label}");
}

/// Asserts `expected` appears in `types` in order (other events may interleave).
pub fn assert_order(types: &[&str], expected: &[&str]) {
    let mut it = types.iter();
    for want in expected {
        assert!(
            it.any(|t| t == want),
            "expected `{want}` after the previous marker in {types:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Domain fixtures
// ---------------------------------------------------------------------------

/// An understanding with no proposed questions.
pub fn understanding(objective: &str) -> Understanding {
    let mut understanding = Understanding::new(objective, "the goal is met");
    understanding.complexity = Complexity::Medium;
    understanding
}

/// An understanding proposing one question.
pub fn understanding_with_question(
    objective: &str,
    text: &str,
    options: &[&str],
    recommended: Option<&str>,
    confidence: f32,
) -> Understanding {
    add_question(
        understanding(objective),
        text,
        options,
        recommended,
        confidence,
    )
}

/// Appends one proposed question to an understanding.
pub fn add_question(
    mut understanding: Understanding,
    text: &str,
    options: &[&str],
    recommended: Option<&str>,
    confidence: f32,
) -> Understanding {
    understanding.proposed_questions.push(ProposedQuestion {
        text: text.to_owned(),
        options: options
            .iter()
            .map(|label| {
                let option = QuestionOption::new(*label);
                if recommended == Some(*label) {
                    option.recommended()
                } else {
                    option
                }
            })
            .collect(),
        multi_select: false,
        why_it_matters: "it changes the plan".to_owned(),
        confidence_if_unasked: confidence,
    });
    understanding
}

/// A plan of `n` independent `implement` tasks titled `t1`…`tn`.
pub fn plan_of(n: usize) -> Plan {
    let tasks = (1..=n)
        .map(|i| {
            let mut task = PlanTask::new(format!("t{i}"), "implement", format!("task {i}"));
            task.instructions = format!("do work {i}");
            task
        })
        .collect();
    Plan::new(tasks, "because")
}

/// A two-task plan where `t2` depends on `t1`.
pub fn plan_chain() -> Plan {
    let mut first = PlanTask::new("t1", "implement", "first task");
    "do the first thing".clone_into(&mut first.instructions);
    let mut second = PlanTask::new("t2", "implement", "second task");
    "do the second thing".clone_into(&mut second.instructions);
    let second = second.depends_on(["t1"]);
    Plan::new(vec![first, second], "chained")
}

/// A plan whose two tasks depend on each other (rejected by `PlanValidator`).
pub fn plan_with_cycle() -> Plan {
    let first = PlanTask::new("t1", "implement", "first").depends_on(["t2"]);
    let second = PlanTask::new("t2", "implement", "second").depends_on(["t1"]);
    Plan::new(vec![first, second], "cyclic")
}

/// The actor of a test-issued command.
pub fn tester() -> Actor {
    Actor::user("tester")
}

// ---------------------------------------------------------------------------
// Services without the saga
// ---------------------------------------------------------------------------

/// The three services over a per-test database, without a `RunActor` — for the
/// scenarios that pin down command handling itself (idempotency, OCC).
pub struct Services {
    /// Per-test database.
    pub db: TestDb,
    /// Temp root.
    pub tmp: TempDir,
    /// The event store.
    pub store: Arc<PgEventStore>,
    /// `Run` commands.
    pub runs: kevin_orchestrator::services::RunService,
    /// `Task` commands.
    pub tasks: kevin_orchestrator::services::TaskService,
    /// `Question` commands.
    pub questions: kevin_orchestrator::services::QuestionService,
    /// Id generator.
    pub ids: Arc<UuidV7IdGen>,
}

impl Services {
    /// Wires the services over a fresh database.
    pub async fn new() -> Self {
        let db = TestDb::new().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(PgEventStore::new(db.pool().clone()));
        let bus = Arc::new(InProcBus::with_defaults());
        let commands = Arc::new(CommandLog::new(db.pool().clone()));
        let ids = Arc::new(UuidV7IdGen);
        let core = kevin_orchestrator::services::ServiceCore::new(
            store.clone(),
            bus,
            commands,
            Arc::new(kevin_domain::SystemClock),
            ids.clone(),
        );
        Self {
            db,
            tmp,
            store,
            runs: kevin_orchestrator::services::RunService::new(core.clone()),
            tasks: kevin_orchestrator::services::TaskService::new(core.clone()),
            questions: kevin_orchestrator::services::QuestionService::new(core),
            ids,
        }
    }

    /// A command context for `run_id` with a fresh command id.
    pub fn ctx(&self, run_id: RunId) -> CommandContext {
        CommandContext::user(self.ids.as_ref(), run_id, "tester")
    }

    /// Every event of `run_id`.
    pub async fn events(&self, run_id: RunId) -> Vec<StoredEvent> {
        self.store
            .read_all(0, 1000)
            .await
            .expect("read_all")
            .into_iter()
            .filter(|e| e.envelope.correlation_id == run_id.as_uuid())
            .collect()
    }

    /// A `StartRun` for `goal`.
    pub fn start_run(&self, run_id: RunId, goal: &str, mode: RunMode) -> StartRun {
        StartRun {
            run_id,
            goal: Goal::new(goal, self.tmp.path()),
            mode,
            budget: default_budget(),
            requested_by: "tester".to_owned(),
            auto_approve_plans: false,
            role_overrides: kevin_domain::RoleOverrides::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Test doubles used by the restart and chaos scenarios
// ---------------------------------------------------------------------------

/// Holds the first attempt of every task (so the runtime can be killed under
/// it) and delegates every later attempt.
pub struct HoldOnce {
    inner: Arc<dyn Worker>,
    seen: std::sync::Mutex<std::collections::HashMap<kevin_domain::TaskId, usize>>,
    started: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for HoldOnce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HoldOnce").finish_non_exhaustive()
    }
}

impl HoldOnce {
    #[must_use]
    pub fn new(inner: Arc<dyn Worker>) -> Self {
        Self {
            inner,
            seen: std::sync::Mutex::new(std::collections::HashMap::new()),
            started: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// How many attempts actually reached the worker.
    #[must_use]
    pub fn started(&self) -> usize {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Worker for HoldOnce {
    fn kind(&self) -> WorkerKind {
        self.inner.kind()
    }

    async fn doctor(&self) -> kevin_worker::Doctor {
        self.inner.doctor().await
    }

    fn validate_alias(
        &self,
        alias: &ModelAlias,
        entry: &ModelEntry,
    ) -> Result<(), kevin_config::ConfigError> {
        self.inner.validate_alias(alias, entry)
    }

    async fn start(
        &self,
        req: kevin_worker::TaskAttemptRequest,
    ) -> Result<kevin_worker::WorkerHandle, kevin_worker::WorkerError> {
        let attempt = {
            let mut seen = self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let count = seen.entry(req.task_id).or_insert(0);
            *count += 1;
            *count
        };
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt > 1 {
            let kind = self.inner.kind();
            let cancel = req.cancel.clone();
            return Ok(kevin_worker::WorkerHandle::spawn(
                kind,
                cancel,
                move |mut sink| async move {
                    sink.emit(kevin_worker::WorkerEvent::Started {
                        session_id: None,
                        pid: None,
                    })
                    .await;
                    sink.emit(kevin_worker::WorkerEvent::Final {
                        text: "done".to_owned(),
                        structured: None,
                        usage: kevin_worker::Usage::default(),
                    })
                    .await;
                    kevin_worker::WorkerOutcome::Succeeded {
                        text: "done".to_owned(),
                        structured: None,
                        usage: kevin_worker::Usage::default(),
                        session_id: None,
                        transcript: kevin_worker::ArtifactRef {
                            id: uuid::Uuid::now_v7(),
                            kind: kevin_worker::ArtifactKind::Transcript,
                            uri: "file:///dev/null".to_owned(),
                            sha256: String::new(),
                            bytes: 0,
                        },
                    }
                },
            ));
        }
        self.inner.start(req).await
    }
}
