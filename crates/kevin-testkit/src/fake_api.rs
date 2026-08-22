//! An in-process fake of the Kevin HTTP API (WS-16).
//!
//! [`FakeRuntime`] implements every `kevin_api::port` trait over a plain
//! in-memory state, so
//!
//! - the router can be driven with `tower::ServiceExt::oneshot` without
//!   Postgres, an orchestrator or a worker subprocess;
//! - `kevin_api::client::KevinClient` and the TUI can be tested against a real
//!   socket ([`spawn`]) that speaks the real wire format;
//! - WS-16 could land before WS-08: the orchestrator implements the same
//!   [`kevin_api::port::RuntimePort`] and swaps in with no HTTP change.
//!
//! Feature: `api`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use kevin_api::auth::TokenVerifier;
use kevin_api::dto::{
    AnswerDto, AnswerRequest, ArtifactDto, BudgetDto, CostQueryDto, CostReportDto, CostRowDto,
    CreateRunRequest, DrainStatusDto, EventDto, GoalDto, LessonsQuery, ListRunsQuery,
    MemoryItemDto, MemorySearchQuery, Page, ProposalDto, ProposalsQuery, QuestionDto,
    QuestionPolicyDto, QuestionPolicyKind, QuestionsQuery, RouteScoreDto, RunDto, RunModeDto,
    RunStatusDto, RunSummaryDto, TaskCountsDto, TaskDto, TaskLogLineDto, TaskLogQueryDto, UsageDto,
    WorkerDoctorDto,
};
use kevin_api::port::{
    ArtifactsPort, CommandCtx, EvaluatorPort, EventsPort, MemoryPort, PortResult, ReadPort,
    Readiness, RouterPort, RuntimeError, RuntimePort, WorkersPort,
};
use kevin_api::state::AppState;
use kevin_bus::{BusStream, EventBus, InProcBus, SubscriptionFilter};
use kevin_domain::ids::{ArtifactId, EventId, MemoryItemId, ProposalId, QuestionId, RunId, TaskId};
use kevin_domain::{Actor, DomainError};
use uuid::Uuid;

/// The bearer token [`state`], [`router`] and [`spawn`] install.
pub const TOKEN: &str = "kevin-testkit-token";

/// A fixed instant so DTO snapshots are stable.
#[must_use]
pub fn fixture_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
        .single()
        .unwrap_or_else(Utc::now)
}

/// Everything the fake serves. Tests mutate it directly through
/// [`FakeRuntime::with_state`].
#[derive(Debug, Default)]
pub struct FakeState {
    /// Runs, keyed by id.
    pub runs: BTreeMap<Uuid, RunDto>,
    /// Tasks, keyed by id.
    pub tasks: BTreeMap<Uuid, TaskDto>,
    /// Questions, keyed by id.
    pub questions: BTreeMap<Uuid, QuestionDto>,
    /// Transcript lines per task.
    pub logs: BTreeMap<Uuid, Vec<TaskLogLineDto>>,
    /// Artifacts and their bytes.
    pub artifacts: BTreeMap<Uuid, (ArtifactDto, Vec<u8>)>,
    /// Persisted events, in position order (`position` = index + 1).
    pub events: Vec<EventDto>,
    /// Routing leaderboard.
    pub routes: Vec<RouteScoreDto>,
    /// Evaluator proposals.
    pub proposals: Vec<ProposalDto>,
    /// Memory items.
    pub memory: Vec<MemoryItemDto>,
    /// Worker doctor rows.
    pub workers: Vec<WorkerDoctorDto>,
    /// The cost report to answer with.
    pub cost: CostReportDto,
    /// Admission gate.
    pub draining: bool,
    /// Whether `/readyz` should report the database as reachable.
    pub db_ok: bool,
    /// Whether `/readyz` should report the workers as healthy.
    pub workers_ok: bool,
    /// Every command the API issued, in order (`"start_run"`, `"cancel_run"`…).
    pub commands: Vec<String>,
    /// Command ids the API passed, in order (the `Idempotency-Key` mapping).
    pub command_ids: Vec<Uuid>,
}

impl FakeState {
    fn new() -> Self {
        Self {
            db_ok: true,
            workers_ok: true,
            ..Self::default()
        }
    }
}

/// In-memory implementation of every `kevin_api::port` trait.
#[derive(Debug, Clone)]
pub struct FakeRuntime {
    state: Arc<Mutex<FakeState>>,
    bus: Arc<InProcBus>,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRuntime {
    /// An empty runtime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState::new())),
            bus: Arc::new(InProcBus::with_defaults()),
        }
    }

    /// The bus the SSE endpoints fan out from.
    #[must_use]
    pub fn bus(&self) -> &Arc<InProcBus> {
        &self.bus
    }

    /// Mutates the fake state.
    pub fn with_state<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(&self, command: &str, ctx: &CommandCtx) {
        let mut state = self.lock();
        state.commands.push(command.to_owned());
        state.command_ids.push(ctx.command_id.as_uuid());
    }

    /// Appends a run so the read endpoints have something to answer with.
    pub fn insert_run(&self, run: RunDto) {
        self.lock().runs.insert(run.id.as_uuid(), run);
    }

    /// Appends a task.
    pub fn insert_task(&self, task: TaskDto) {
        self.lock().tasks.insert(task.id.as_uuid(), task);
    }

    /// Appends a question.
    pub fn insert_question(&self, question: QuestionDto) {
        self.lock()
            .questions
            .insert(question.id.as_uuid(), question);
    }

    /// Appends transcript lines to a task.
    pub fn insert_log(&self, task_id: TaskId, lines: Vec<TaskLogLineDto>) {
        self.lock()
            .logs
            .entry(task_id.as_uuid())
            .or_default()
            .extend(lines);
    }

    /// Appends an artifact and its bytes.
    pub fn insert_artifact(&self, artifact: ArtifactDto, bytes: Vec<u8>) {
        self.lock()
            .artifacts
            .insert(artifact.id.as_uuid(), (artifact, bytes));
    }

    /// Appends an event to the history **and** fans it out on the bus, so
    /// catch-up and live delivery agree on positions.
    pub async fn publish(&self, event_type: &'static str, run_id: RunId) -> u64 {
        let version = u64::try_from(self.lock().events.len() + 1).unwrap_or(1);
        let envelope = crate::bus::event(
            run_id.as_uuid(),
            "run",
            run_id.as_uuid(),
            event_type,
            version,
        );
        self.bus.publish(std::slice::from_ref(&envelope)).await.ok();
        let position = self.bus.position();
        let dto = EventDto {
            position,
            event_id: EventId::from_uuid(envelope.event_id.as_uuid()),
            event_type: envelope.event_type.to_owned(),
            occurred_at: envelope.occurred_at,
            aggregate_type: envelope.aggregate_type.to_owned(),
            aggregate_id: envelope.aggregate_id,
            aggregate_version: envelope.aggregate_version,
            correlation_id: envelope.correlation_id,
            payload: envelope.payload.clone(),
        };
        self.lock().events.push(dto);
        position
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A `RunDto` in `awaiting_plan_approval` with no tasks.
#[must_use]
pub fn run_fixture(id: RunId) -> RunDto {
    RunDto {
        id,
        status: RunStatusDto::AwaitingPlanApproval,
        goal: GoalDto {
            text: "Add a /healthz endpoint to the axum app and tests".to_owned(),
            attachments: Vec::new(),
            cwd: std::path::PathBuf::from("/repo"),
            repo_kind: "git".to_owned(),
        },
        mode: RunModeDto::Interactive,
        budget: BudgetDto {
            max_attempts: 2,
            max_parallel: 4,
            ..BudgetDto::default()
        },
        usage: UsageDto::default(),
        understanding: None,
        plan: None,
        open_questions: Vec::new(),
        tasks: Vec::new(),
        evaluation: None,
        created_at: fixture_time(),
        updated_at: fixture_time(),
        version: 3,
    }
}

/// A `TaskDto` in `pending` belonging to `run_id`.
#[must_use]
pub fn task_fixture(id: TaskId, run_id: RunId) -> TaskDto {
    TaskDto {
        id,
        run_id,
        kind: "implement".to_owned(),
        title: "Add /healthz".to_owned(),
        status: "pending".to_owned(),
        route: None,
        attempts: Vec::new(),
        depends_on: Vec::new(),
        usage: UsageDto::default(),
        artifacts: Vec::new(),
        acceptance_criteria: vec!["GET /healthz returns 200".to_owned()],
    }
}

/// An open `QuestionDto` belonging to `run_id`.
#[must_use]
pub fn question_fixture(id: QuestionId, run_id: RunId) -> QuestionDto {
    QuestionDto {
        id,
        run_id,
        task_id: None,
        text: "Which framework version?".to_owned(),
        options: Vec::new(),
        multi_select: false,
        default: None,
        policy: QuestionPolicyDto {
            kind: QuestionPolicyKind::Block,
            timeout_ms: None,
        },
        status: "open".to_owned(),
        answer: None,
        asked_at: fixture_time(),
    }
}

fn summary(run: &RunDto) -> RunSummaryDto {
    RunSummaryDto {
        id: run.id,
        status: run.status,
        goal_excerpt: run.goal.text.lines().next().unwrap_or("").to_owned(),
        usage: run.usage.clone(),
        task_counts: TaskCountsDto {
            total: u32::try_from(run.tasks.len()).unwrap_or(u32::MAX),
            ..TaskCountsDto::default()
        },
        created_at: run.created_at,
        updated_at: run.updated_at,
    }
}

// ---------------------------------------------------------------------------
// RuntimePort
// ---------------------------------------------------------------------------

#[async_trait]
impl RuntimePort for FakeRuntime {
    async fn start_run(&self, request: CreateRunRequest, ctx: CommandCtx) -> PortResult<RunDto> {
        self.record("start_run", &ctx);
        if self.lock().draining {
            return Err(RuntimeError::Draining);
        }
        // The command id is derived from the `Idempotency-Key`, so a replay
        // deterministically produces the same run id — exactly what
        // `core.processed_commands` guarantees in the real runtime.
        let id = RunId::from_uuid(ctx.command_id.as_uuid());
        let mut run = run_fixture(id);
        run.status = RunStatusDto::Received;
        run.goal.text = request.goal;
        if let Some(cwd) = request.cwd {
            run.goal.cwd = cwd;
        }
        run.mode = request.mode.unwrap_or_default();
        if let Some(budget) = request.budget {
            run.budget = budget;
        }
        run.version = 1;
        self.insert_run(run.clone());
        Ok(run)
    }

    async fn cancel_run(
        &self,
        run_id: RunId,
        _reason: Option<String>,
        ctx: CommandCtx,
    ) -> PortResult<RunDto> {
        self.record("cancel_run", &ctx);
        self.mutate_run(run_id, |run| {
            run.status = RunStatusDto::Cancelled;
        })
    }

    async fn approve_plan(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<RunDto> {
        self.record("approve_plan", &ctx);
        self.mutate_run(run_id, |run| {
            run.status = RunStatusDto::Executing;
        })
    }

    async fn reject_plan(
        &self,
        run_id: RunId,
        _feedback: String,
        ctx: CommandCtx,
    ) -> PortResult<RunDto> {
        self.record("reject_plan", &ctx);
        self.mutate_run(run_id, |run| {
            run.status = RunStatusDto::Planning;
        })
    }

    async fn evaluate_run(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<()> {
        self.record("evaluate_run", &ctx);
        if self.lock().runs.contains_key(&run_id.as_uuid()) {
            return Ok(());
        }
        Err(RuntimeError::RunNotFound(run_id))
    }

    async fn retry_task(
        &self,
        task_id: TaskId,
        _exclude_route: bool,
        ctx: CommandCtx,
    ) -> PortResult<TaskDto> {
        self.record("retry_task", &ctx);
        self.mutate_task(task_id, |task| {
            "routed".clone_into(&mut task.status);
        })
    }

    async fn cancel_task(&self, task_id: TaskId, ctx: CommandCtx) -> PortResult<TaskDto> {
        self.record("cancel_task", &ctx);
        self.mutate_task(task_id, |task| {
            "cancelled".clone_into(&mut task.status);
        })
    }

    async fn answer_question(
        &self,
        question_id: QuestionId,
        answer: AnswerRequest,
        ctx: CommandCtx,
    ) -> PortResult<QuestionDto> {
        self.record("answer_question", &ctx);
        let mut state = self.lock();
        let question = state
            .questions
            .get_mut(&question_id.as_uuid())
            .ok_or(RuntimeError::QuestionNotFound(question_id))?;
        if question.status == "answered" {
            return Err(RuntimeError::Domain(DomainError::AlreadyAnswered));
        }
        "answered".clone_into(&mut question.status);
        question.answer = Some(AnswerDto {
            selected: answer.selected,
            free_text: answer.free_text,
            answered_by: "api".to_owned(),
        });
        Ok(question.clone())
    }

    async fn set_drain(&self, draining: bool) -> PortResult<DrainStatusDto> {
        self.lock().draining = draining;
        self.drain_status().await
    }

    async fn drain_status(&self) -> PortResult<DrainStatusDto> {
        let state = self.lock();
        Ok(DrainStatusDto {
            draining: state.draining,
            running_runs: u32::try_from(
                state
                    .runs
                    .values()
                    .filter(|run| run.status == RunStatusDto::Executing)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            running_attempts: 0,
        })
    }

    async fn readiness(&self) -> Readiness {
        let state = self.lock();
        Readiness {
            db: state.db_ok,
            draining: state.draining,
            workers_ok: state.workers_ok,
        }
    }
}

impl FakeRuntime {
    fn mutate_run(&self, run_id: RunId, f: impl FnOnce(&mut RunDto)) -> PortResult<RunDto> {
        let mut state = self.lock();
        let run = state
            .runs
            .get_mut(&run_id.as_uuid())
            .ok_or(RuntimeError::RunNotFound(run_id))?;
        f(run);
        run.version += 1;
        Ok(run.clone())
    }

    fn mutate_task(&self, task_id: TaskId, f: impl FnOnce(&mut TaskDto)) -> PortResult<TaskDto> {
        let mut state = self.lock();
        let task = state
            .tasks
            .get_mut(&task_id.as_uuid())
            .ok_or(RuntimeError::TaskNotFound(task_id))?;
        f(task);
        Ok(task.clone())
    }
}

// ---------------------------------------------------------------------------
// ReadPort
// ---------------------------------------------------------------------------

#[async_trait]
impl ReadPort for FakeRuntime {
    async fn run(&self, run_id: RunId) -> PortResult<Option<RunDto>> {
        Ok(self.lock().runs.get(&run_id.as_uuid()).cloned())
    }

    async fn runs(&self, query: &ListRunsQuery) -> PortResult<Page<RunSummaryDto>> {
        let state = self.lock();
        let items = state
            .runs
            .values()
            .filter(|run| {
                query.status.as_deref().is_none_or(|status| {
                    serde_json::to_value(run.status)
                        .ok()
                        .and_then(|v| v.as_str().map(ToOwned::to_owned))
                        .is_some_and(|name| name == status)
                })
            })
            .take(query.limit.unwrap_or(50))
            .map(summary)
            .collect();
        Ok(Page::new(items))
    }

    async fn tasks_of_run(&self, run_id: RunId) -> PortResult<Vec<TaskDto>> {
        Ok(self
            .lock()
            .tasks
            .values()
            .filter(|task| task.run_id == run_id)
            .cloned()
            .collect())
    }

    async fn task(&self, task_id: TaskId) -> PortResult<Option<TaskDto>> {
        Ok(self.lock().tasks.get(&task_id.as_uuid()).cloned())
    }

    async fn task_log(
        &self,
        task_id: TaskId,
        query: &TaskLogQueryDto,
    ) -> PortResult<Page<TaskLogLineDto>> {
        let state = self.lock();
        let items = state
            .logs
            .get(&task_id.as_uuid())
            .map(|lines| {
                lines
                    .iter()
                    .filter(|line| query.attempt.is_none_or(|a| a == line.attempt))
                    .filter(|line| query.after_seq.is_none_or(|after| line.seq > after))
                    .take(query.limit.unwrap_or(50))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(Page::new(items))
    }

    async fn artifacts_of_task(&self, task_id: TaskId) -> PortResult<Vec<ArtifactDto>> {
        Ok(self
            .lock()
            .artifacts
            .values()
            .filter(|(artifact, _)| artifact.task_id == Some(task_id))
            .map(|(artifact, _)| artifact.clone())
            .collect())
    }

    async fn artifact(&self, artifact_id: ArtifactId) -> PortResult<Option<ArtifactDto>> {
        Ok(self
            .lock()
            .artifacts
            .get(&artifact_id.as_uuid())
            .map(|(artifact, _)| artifact.clone()))
    }

    async fn question(&self, question_id: QuestionId) -> PortResult<Option<QuestionDto>> {
        Ok(self.lock().questions.get(&question_id.as_uuid()).cloned())
    }

    async fn questions(&self, query: &QuestionsQuery) -> PortResult<Page<QuestionDto>> {
        let state = self.lock();
        let items = state
            .questions
            .values()
            .filter(|q| query.run_id.is_none_or(|run_id| q.run_id == run_id))
            .filter(|q| query.status.as_deref().is_none_or(|s| q.status == s))
            .take(query.limit.unwrap_or(50))
            .cloned()
            .collect();
        Ok(Page::new(items))
    }

    async fn cost(&self, query: &CostQueryDto) -> PortResult<CostReportDto> {
        let state = self.lock();
        let mut report = state.cost.clone();
        if query.group_by.as_deref() == Some("model") {
            report.rows = report
                .rows
                .iter()
                .map(|row| CostRowDto {
                    key: "model".to_owned(),
                    ..row.clone()
                })
                .collect();
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// EventsPort
// ---------------------------------------------------------------------------

#[async_trait]
impl EventsPort for FakeRuntime {
    async fn after(&self, from: u64, limit: usize) -> PortResult<Vec<EventDto>> {
        Ok(self
            .lock()
            .events
            .iter()
            .filter(|event| event.position > from)
            .take(limit)
            .cloned()
            .collect())
    }

    fn subscribe_live(&self) -> BusStream {
        self.bus.subscribe(SubscriptionFilter::all().named("fake"))
    }

    fn head(&self) -> u64 {
        self.bus.position()
    }
}

// ---------------------------------------------------------------------------
// Side ports
// ---------------------------------------------------------------------------

#[async_trait]
impl RouterPort for FakeRuntime {
    async fn leaderboard(&self, kind: Option<&str>) -> PortResult<Vec<RouteScoreDto>> {
        Ok(self
            .lock()
            .routes
            .iter()
            .filter(|score| kind.is_none_or(|k| score.kind == k))
            .cloned()
            .collect())
    }
}

#[async_trait]
impl EvaluatorPort for FakeRuntime {
    async fn proposals(&self, query: &ProposalsQuery) -> PortResult<Page<ProposalDto>> {
        let state = self.lock();
        let items = state
            .proposals
            .iter()
            .filter(|p| query.status.as_deref().is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        Ok(Page::new(items))
    }

    async fn decide_proposal(
        &self,
        proposal_id: ProposalId,
        accept: bool,
        _note: Option<String>,
        ctx: CommandCtx,
    ) -> PortResult<ProposalDto> {
        self.record(
            if accept {
                "accept_proposal"
            } else {
                "reject_proposal"
            },
            &ctx,
        );
        let mut state = self.lock();
        let proposal = state
            .proposals
            .iter_mut()
            .find(|p| p.id == proposal_id)
            .ok_or(RuntimeError::ProposalNotFound(proposal_id))?;
        if accept { "accepted" } else { "rejected" }.clone_into(&mut proposal.status);
        Ok(proposal.clone())
    }
}

#[async_trait]
impl MemoryPort for FakeRuntime {
    async fn search(&self, query: &MemorySearchQuery) -> PortResult<Vec<MemoryItemDto>> {
        let needle = query.q.to_lowercase();
        Ok(self
            .lock()
            .memory
            .iter()
            .filter(|item| item.content.to_lowercase().contains(&needle))
            .take(query.top_k.unwrap_or(10))
            .cloned()
            .collect())
    }

    async fn lessons(&self, query: &LessonsQuery) -> PortResult<Page<MemoryItemDto>> {
        let state = self.lock();
        let items = state
            .memory
            .iter()
            .filter(|item| item.kind == "lesson")
            .take(query.limit.unwrap_or(50))
            .cloned()
            .collect();
        Ok(Page::new(items))
    }

    async fn forget(&self, item_id: MemoryItemId, _actor: Actor) -> PortResult<()> {
        self.lock().memory.retain(|item| item.id != item_id);
        Ok(())
    }
}

#[async_trait]
impl WorkersPort for FakeRuntime {
    async fn doctor(&self) -> PortResult<Vec<WorkerDoctorDto>> {
        Ok(self.lock().workers.clone())
    }
}

#[async_trait]
impl ArtifactsPort for FakeRuntime {
    async fn read(&self, artifact: &ArtifactDto) -> PortResult<(String, Vec<u8>)> {
        let state = self.lock();
        let (_, bytes) = state
            .artifacts
            .get(&artifact.id.as_uuid())
            .ok_or(RuntimeError::ArtifactNotFound(artifact.id))?;
        Ok((
            kevin_api::adapters::content_type_of(&artifact.kind).to_owned(),
            bytes.clone(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

/// An [`AppState`] with every port served by `runtime` and [`TOKEN`] as the
/// bearer token.
#[must_use]
pub fn state(runtime: &FakeRuntime) -> AppState {
    let runtime = Arc::new(runtime.clone());
    AppState::builder(
        Arc::clone(&runtime) as Arc<dyn RuntimePort>,
        Arc::clone(&runtime) as Arc<dyn ReadPort>,
        Arc::clone(&runtime) as Arc<dyn EventsPort>,
        Arc::new(TokenVerifier::new(TOKEN)),
    )
    .router_port(Arc::clone(&runtime) as Arc<dyn RouterPort>)
    .evaluator(Arc::clone(&runtime) as Arc<dyn EvaluatorPort>)
    .memory(Arc::clone(&runtime) as Arc<dyn MemoryPort>)
    .workers(Arc::clone(&runtime) as Arc<dyn WorkersPort>)
    .artifacts(runtime as Arc<dyn ArtifactsPort>)
    .build()
}

/// The full API router in front of `runtime` (`tower::ServiceExt::oneshot`).
pub fn router(runtime: &FakeRuntime) -> axum::Router {
    kevin_api::router(state(runtime))
}

/// A running fake API on an ephemeral loopback port.
#[derive(Debug)]
pub struct ServerHandle {
    /// Where the server is listening.
    pub addr: SocketAddr,
    /// The bearer token clients must present.
    pub token: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    /// `http://127.0.0.1:<port>/`, ready for `KevinClient::connect`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// Stops the server and waits for the task to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.await;
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// Serves [`router`] on `127.0.0.1:0` until the handle is dropped.
///
/// # Panics
///
/// Panics when the loopback port cannot be bound, which only happens if the
/// test host has no loopback interface.
pub async fn spawn(runtime: &FakeRuntime) -> ServerHandle {
    let app = router(runtime);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind an ephemeral loopback port");
    let addr = listener.local_addr().expect("local address");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let join = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = rx.await;
        })
        .await;
    });
    ServerHandle {
        addr,
        token: TOKEN.to_owned(),
        shutdown: Some(tx),
        join: Some(join),
    }
}
