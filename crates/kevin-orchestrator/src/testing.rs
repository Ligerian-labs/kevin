//! In-process fakes of every port (`plan/11-testing.md` §Determinism rules).
//!
//! WS-09 (`kevin-router`), WS-10 (`roles`), WS-18 (`kevin-memory`) and WS-19
//! (`kevin-evaluator`) are not merged yet, so the engine's tests — and the
//! tests of the crates above it — wire these instead. They are deliberately
//! scripted and side-effect free: no subprocess, no repository, no network.
//!
//! This module is compiled into the library (not behind `cfg(test)`) so
//! `kevin-api`, `kevin-cli` and `kevin-kohral` can build a working
//! orchestrator in their own integration tests.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use kevin_domain::run::RunEvaluation;
use kevin_domain::{
    ArtifactId, ArtifactKind, ArtifactRef, CommandId, FailureClass, Goal, ModelAlias, Plan, Route,
    RunId, TaskId, Understanding, Usage, Workspace, WorkspaceKind,
};
use kevin_store::StoreError;
use serde_json::Value;

use crate::ports::{
    AnsweredQuestion, CommandIdempotency, EvaluatorPort, IntegrateContext, IntegrateRequest,
    IntegrationOutcome, IntegrationSummary, Lesson, MemoryPort, PortError, PortResult,
    PrepareWorkspace, RecordRouteOutcome, RoleContext, RolesPort, RouteSelection, RouterPort,
    SelectRouteQuery, WorkspacePort,
};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// Command idempotency
// ---------------------------------------------------------------------------

/// [`CommandIdempotency`] backed by a map — for tests that do not want the
/// `core.processed_commands` table.
#[derive(Debug, Default)]
pub struct InMemoryCommands {
    results: Mutex<HashMap<CommandId, Value>>,
}

impl InMemoryCommands {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of recorded commands.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.results).len()
    }

    /// `true` when nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl CommandIdempotency for InMemoryCommands {
    async fn begin(&self, command_id: CommandId) -> Result<Option<Value>, StoreError> {
        Ok(lock(&self.results).get(&command_id).cloned())
    }

    async fn complete(&self, command_id: CommandId, result: &Value) -> Result<Value, StoreError> {
        let mut results = lock(&self.results);
        Ok(results
            .entry(command_id)
            .or_insert_with(|| result.clone())
            .clone())
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// A router that hands out routes from a fixed list, honouring `exclude`
/// (so retry-reroutes are observable) and recording every outcome.
#[derive(Debug)]
pub struct FixedRouter {
    routes: Vec<Route>,
    outcomes: Mutex<Vec<RecordRouteOutcome>>,
    selections: Mutex<Vec<SelectRouteQuery>>,
}

impl FixedRouter {
    /// A router over `routes`, first one preferred.
    #[must_use]
    pub fn new(routes: Vec<Route>) -> Self {
        Self {
            routes,
            outcomes: Mutex::new(Vec::new()),
            selections: Mutex::new(Vec::new()),
        }
    }

    /// A router with a single route.
    #[must_use]
    pub fn single(route: Route) -> Self {
        Self::new(vec![route])
    }

    /// Every recorded outcome, in order.
    #[must_use]
    pub fn outcomes(&self) -> Vec<RecordRouteOutcome> {
        lock(&self.outcomes).clone()
    }

    /// Every selection query, in order.
    #[must_use]
    pub fn selections(&self) -> Vec<SelectRouteQuery> {
        lock(&self.selections).clone()
    }
}

#[async_trait]
impl RouterPort for FixedRouter {
    async fn select(&self, query: SelectRouteQuery) -> PortResult<RouteSelection> {
        lock(&self.selections).push(query.clone());
        let route = self
            .routes
            .iter()
            .find(|r| !query.exclude.contains(&r.model))
            .or_else(|| self.routes.first())
            .cloned()
            .ok_or_else(|| PortError::permanent("router", "no candidate routes"))?;
        Ok(RouteSelection::fixed(route))
    }

    async fn record_outcome(&self, outcome: RecordRouteOutcome) -> PortResult<()> {
        lock(&self.outcomes).push(outcome);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// A scripted answer of one role call.
type Scripted<T> = Result<(T, Usage), (String, FailureClass)>;

/// Planner and integrator answers, scripted per call.
///
/// The queues pop one entry per call and keep repeating the last one, so
/// `with_plan(a).with_plan(b)` answers `a` then `b` then `b`…
#[derive(Debug, Default)]
pub struct ScriptedRoles {
    understandings: Mutex<VecDeque<Scripted<Understanding>>>,
    plans: Mutex<VecDeque<Scripted<Plan>>>,
    summary: Mutex<String>,
    calls: Mutex<Vec<&'static str>>,
    plan_contexts: Mutex<Vec<PlanCall>>,
}

/// What a `plan` call was given (assertions on feedback / repair loops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCall {
    /// Answers the planner saw.
    pub answers: Vec<AnsweredQuestion>,
    /// Validation errors of the previous plan (repair call).
    pub repair_errors: Vec<String>,
    /// Reviewer feedback after `run.plan_rejected`.
    pub feedback: Option<String>,
    /// The memory block, when memory was wired.
    pub memory: Option<String>,
    /// Platform briefing section titles.
    pub system_context: Vec<String>,
}

impl ScriptedRoles {
    /// Empty script (every call fails until something is queued).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one `understanding` answer.
    #[must_use]
    pub fn with_understanding(self, understanding: Understanding) -> Self {
        lock(&self.understandings).push_back(Ok((understanding, Usage::ZERO)));
        self
    }

    /// Queues one failing `understanding` answer.
    #[must_use]
    pub fn with_understanding_error(self, message: &str, class: FailureClass) -> Self {
        lock(&self.understandings).push_back(Err((message.to_owned(), class)));
        self
    }

    /// Queues one `plan` answer.
    #[must_use]
    pub fn with_plan(self, plan: Plan) -> Self {
        lock(&self.plans).push_back(Ok((plan, Usage::ZERO)));
        self
    }

    /// Queues one `plan` answer with usage (budget tests).
    #[must_use]
    pub fn with_plan_usage(self, plan: Plan, usage: Usage) -> Self {
        lock(&self.plans).push_back(Ok((plan, usage)));
        self
    }

    /// Sets the integrator summary.
    #[must_use]
    pub fn with_summary(self, summary: &str) -> Self {
        summary.clone_into(&mut lock(&self.summary));
        self
    }

    /// Names of the calls made, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<&'static str> {
        lock(&self.calls).clone()
    }

    /// How many `plan` calls were made.
    #[must_use]
    pub fn plan_calls(&self) -> usize {
        lock(&self.calls).iter().filter(|c| **c == "plan").count()
    }

    /// What each `plan` call was given.
    #[must_use]
    pub fn plan_contexts(&self) -> Vec<PlanCall> {
        lock(&self.plan_contexts).clone()
    }

    fn pop<T: Clone>(
        queue: &Mutex<VecDeque<Scripted<T>>>,
        role: &'static str,
    ) -> PortResult<(T, Usage)> {
        let mut queue = lock(queue);
        let entry = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        };
        match entry {
            Some(Ok(value)) => Ok(value),
            Some(Err((message, class))) => Err(PortError {
                port: "roles",
                message,
                class,
            }),
            None => Err(PortError::permanent(
                "roles",
                format!("no scripted answer for the `{role}` role"),
            )),
        }
    }
}

#[async_trait]
impl RolesPort for ScriptedRoles {
    async fn understanding(&self, _ctx: &RoleContext) -> PortResult<(Understanding, Usage)> {
        lock(&self.calls).push("understanding");
        Self::pop(&self.understandings, "understanding")
    }

    async fn plan(&self, ctx: &RoleContext) -> PortResult<(Plan, Usage)> {
        lock(&self.calls).push("plan");
        lock(&self.plan_contexts).push(PlanCall {
            answers: ctx.answers.clone(),
            repair_errors: ctx.repair_errors.clone(),
            feedback: ctx.feedback.clone(),
            memory: ctx.memory.clone(),
            system_context: ctx
                .system_context
                .iter()
                .map(|section| section.title.clone())
                .collect(),
        });
        Self::pop(&self.plans, "plan")
    }

    async fn integrate(&self, _ctx: &IntegrateContext) -> PortResult<(IntegrationSummary, Usage)> {
        lock(&self.calls).push("integrate");
        let summary = lock(&self.summary).clone();
        let summary = if summary.is_empty() {
            "integrated".to_owned()
        } else {
            summary
        };
        Ok((IntegrationSummary { summary }, Usage::ZERO))
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

/// A [`WorkspacePort`] that hands out directories under one root and answers
/// integration from a script.
#[derive(Debug)]
pub struct TempWorkspaces {
    root: PathBuf,
    integrations: Mutex<VecDeque<IntegrationOutcome>>,
    prepared: AtomicUsize,
    cleaned: AtomicUsize,
    integrated: AtomicUsize,
}

impl TempWorkspaces {
    /// Workspaces under `root` (a `tempfile::TempDir` path in tests).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            integrations: Mutex::new(VecDeque::new()),
            prepared: AtomicUsize::new(0),
            cleaned: AtomicUsize::new(0),
            integrated: AtomicUsize::new(0),
        }
    }

    /// Queues one integration answer (the last one repeats).
    #[must_use]
    pub fn with_integration(self, outcome: IntegrationOutcome) -> Self {
        lock(&self.integrations).push_back(outcome);
        self
    }

    /// Queues an integration answer reporting `conflicts`.
    #[must_use]
    pub fn with_conflicts(self, conflicts: &[&str]) -> Self {
        self.with_integration(IntegrationOutcome {
            artifacts: Vec::new(),
            conflicts: conflicts.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    /// How many workspaces were prepared.
    #[must_use]
    pub fn prepared(&self) -> usize {
        self.prepared.load(Ordering::SeqCst)
    }

    /// How many workspaces were cleaned up.
    #[must_use]
    pub fn cleaned(&self) -> usize {
        self.cleaned.load(Ordering::SeqCst)
    }

    /// How many integration calls were made.
    #[must_use]
    pub fn integrations(&self) -> usize {
        self.integrated.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl WorkspacePort for TempWorkspaces {
    async fn prepare(&self, req: PrepareWorkspace) -> PortResult<Workspace> {
        let dir = self
            .root
            .join(req.run_id.to_string())
            .join(req.task_id.to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| PortError::transient("workspace", e.to_string()))?;
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(Workspace {
            root: dir,
            kind: WorkspaceKind::GitWorktree {
                branch: format!("kevin/{}", req.task_id),
            },
            base_rev: None,
        })
    }

    async fn cleanup(&self, _workspace: &Workspace, _succeeded: bool) -> PortResult<()> {
        self.cleaned.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn integrate(&self, req: IntegrateRequest) -> PortResult<IntegrationOutcome> {
        self.integrated.fetch_add(1, Ordering::SeqCst);
        let mut queue = lock(&self.integrations);
        let outcome = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().cloned()
        };
        Ok(outcome.unwrap_or_else(|| IntegrationOutcome {
            artifacts: vec![ArtifactRef {
                id: ArtifactId::new(),
                kind: ArtifactKind::PrUrl,
                uri: format!("https://example.invalid/pr/{}", req.run_id),
                sha256: None,
                bytes: None,
            }],
            conflicts: Vec::new(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Memory / evaluator / system context
// ---------------------------------------------------------------------------

/// A memory port that answers a canned context and records lessons.
#[derive(Debug, Default)]
pub struct RecordingMemory {
    context: Option<String>,
    lessons: Mutex<Vec<Lesson>>,
}

impl RecordingMemory {
    /// A memory that returns `context` for every intake.
    #[must_use]
    pub fn with_context(context: &str) -> Self {
        Self {
            context: Some(context.to_owned()),
            lessons: Mutex::new(Vec::new()),
        }
    }

    /// Lessons stored so far.
    #[must_use]
    pub fn lessons(&self) -> Vec<Lesson> {
        lock(&self.lessons).clone()
    }
}

#[async_trait]
impl MemoryPort for RecordingMemory {
    async fn context_for_intake(
        &self,
        _goal: &Goal,
        _repo: Option<&str>,
    ) -> PortResult<Option<String>> {
        Ok(self.context.clone())
    }

    async fn store_lesson(&self, lesson: Lesson) -> PortResult<()> {
        lock(&self.lessons).push(lesson);
        Ok(())
    }
}

/// A judge that answers after `delay` (use a delay above
/// `orchestrator.evaluation_timeout` to exercise the timeout path).
#[derive(Debug)]
pub struct ScriptedEvaluator {
    evaluation: Option<RunEvaluation>,
    delay: Duration,
    calls: AtomicUsize,
}

impl ScriptedEvaluator {
    /// Answers `evaluation` immediately.
    #[must_use]
    pub fn new(evaluation: Option<RunEvaluation>) -> Self {
        Self {
            evaluation,
            delay: Duration::ZERO,
            calls: AtomicUsize::new(0),
        }
    }

    /// Answers after `delay`.
    #[must_use]
    pub fn slow(delay: Duration) -> Self {
        Self {
            evaluation: None,
            delay,
            calls: AtomicUsize::new(0),
        }
    }

    /// How many times the judge was called.
    #[must_use]
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EvaluatorPort for ScriptedEvaluator {
    async fn evaluate_run(
        &self,
        _run_id: RunId,
        _task_ids: &[TaskId],
    ) -> PortResult<Option<RunEvaluation>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(self.evaluation.clone())
    }
}

/// The `fake` route every fake-worker test uses.
///
/// # Panics
/// Never: `"fake"` is a valid [`ModelAlias`].
#[must_use]
pub fn fake_route() -> Route {
    Route::new(
        kevin_domain::WorkerKind::Fake,
        ModelAlias::new("fake").expect("`fake` is a valid alias"),
    )
}

/// A second `fake` alias, so retry-reroutes pick a different one.
///
/// # Panics
/// Never: `"fake-alt"` is a valid [`ModelAlias`].
#[must_use]
pub fn fake_alt_route() -> Route {
    Route::new(
        kevin_domain::WorkerKind::Fake,
        ModelAlias::new("fake-alt").expect("`fake-alt` is a valid alias"),
    )
}

// ---------------------------------------------------------------------------
// Flaky worker
// ---------------------------------------------------------------------------

/// Wraps another [`Worker`] and fails the first `fail_first` attempts **per
/// task** before delegating.
///
/// The `fake` worker picks its answer from the prompt, which is identical on
/// every retry; this wrapper is what makes "attempt 1 fails, attempt 2
/// succeeds" expressible without a model (`plan/05-orchestration.md` §6
/// `transient_retry_reroutes`).
pub struct FlakyWorker {
    inner: std::sync::Arc<dyn kevin_worker::Worker>,
    fail_first: usize,
    class: FailureClass,
    message: String,
    seen: Mutex<HashMap<TaskId, usize>>,
}

impl std::fmt::Debug for FlakyWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlakyWorker")
            .field("kind", &self.inner.kind())
            .field("fail_first", &self.fail_first)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl FlakyWorker {
    /// Fails the first `fail_first` attempts of every task with `class`.
    #[must_use]
    pub fn new(
        inner: std::sync::Arc<dyn kevin_worker::Worker>,
        fail_first: usize,
        class: FailureClass,
    ) -> Self {
        Self {
            inner,
            fail_first,
            class,
            message: "simulated flake".to_owned(),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Attempts started per task.
    #[must_use]
    pub fn attempts(&self, task_id: TaskId) -> usize {
        lock(&self.seen).get(&task_id).copied().unwrap_or_default()
    }
}

#[async_trait]
impl kevin_worker::Worker for FlakyWorker {
    fn kind(&self) -> kevin_domain::WorkerKind {
        self.inner.kind()
    }

    async fn doctor(&self) -> kevin_worker::Doctor {
        self.inner.doctor().await
    }

    fn validate_alias(
        &self,
        alias: &ModelAlias,
        entry: &kevin_config::ModelEntry,
    ) -> Result<(), kevin_config::ConfigError> {
        self.inner.validate_alias(alias, entry)
    }

    async fn start(
        &self,
        req: kevin_worker::TaskAttemptRequest,
    ) -> Result<kevin_worker::WorkerHandle, kevin_worker::WorkerError> {
        let attempt = {
            let mut seen = lock(&self.seen);
            let count = seen.entry(req.task_id).or_insert(0);
            *count += 1;
            *count
        };
        if attempt > self.fail_first {
            return self.inner.start(req).await;
        }
        let kind = self.inner.kind();
        let class = self.class;
        let message = self.message.clone();
        let cancel = req.cancel.clone();
        Ok(kevin_worker::WorkerHandle::spawn(
            kind,
            cancel,
            move |mut sink| async move {
                sink.emit(kevin_worker::WorkerEvent::Started {
                    session_id: None,
                    pid: None,
                })
                .await;
                sink.emit(kevin_worker::WorkerEvent::Failed {
                    class,
                    message: message.clone(),
                    usage: kevin_worker::Usage::default(),
                })
                .await;
                kevin_worker::WorkerOutcome::failed(class, message, kevin_worker::Usage::default())
            },
        ))
    }
}
