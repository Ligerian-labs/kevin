//! The embedded runtime (`plan/07-api-and-tui.md` §3, `plan/10-observability-ops.md`
//! §Startup and shutdown).
//!
//! When `client.server_url` is empty the CLI *is* the runtime: it opens the
//! database, runs the migrations, wires every [`Deps`] port to its real
//! implementation and boots the orchestrator in-process.
//!
//! Two shapes are provided, because most subcommands do not need a saga:
//!
//! - [`Backend`] — configuration, pool, event store, bus, command services and
//!   the `orch.*` read models. Enough for `kevin runs ls`, `kevin answer`,
//!   `kevin cost`, … A one-shot command that *appends* events calls
//!   [`Backend::catch_up`] afterwards so the read models it just changed are
//!   current before it prints.
//! - [`EmbeddedRuntime`] — a [`Backend`] plus the projection runners and a
//!   booted [`Orchestrator`]. This is what `kevin run` drives.
//!
//! Serving the HTTP API is deliberately **not** part of this module: WS-16
//! (`kevin-api`) is still in flight, so `EmbeddedRuntime` exposes the pieces an
//! `AppState` needs ([`EmbeddedRuntime::handle`], [`Backend::read_models`],
//! [`Backend::bus`]) and WS-20 adds the listener on top for `kevin serve`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use kevin_bus::{EventBus, InProcBus, PgNotifyBus};
use kevin_config::KevinConfig;
use kevin_domain::route_score::RecordRouteOutcome as DomainRouteOutcome;
use kevin_domain::{Clock, Goal, IdGen, SystemClock, UuidV7IdGen};
use kevin_evaluator::{AutoApply, Evaluator, EvaluatorConfig, PgEvaluationRepo};
use kevin_memory::{ContextBuilder, MemoryCfg, MemoryStore, RepoId, StoreRequest, embed};
use kevin_orchestrator::evaluator_port::EvaluationRunner;
use kevin_orchestrator::orchestrator::{DEFAULT_TICK, Deps, Handle, Orchestrator};
use kevin_orchestrator::ports::{
    CandidateScore, EvaluatorPort, Lesson, MemoryPort, PortError, PortResult, RecordRouteOutcome,
    RouteSelection, RouterPort, SelectRouteQuery,
};
use kevin_orchestrator::projections;
use kevin_orchestrator::projections::{ProjectionRunner, ReadModels, TaskLog};
use kevin_orchestrator::role_port::RoleRunnerRoles;
use kevin_orchestrator::roles::SystemContextProvider;
use kevin_orchestrator::services::{QuestionService, RunService, ServiceCore, TaskService};
use kevin_orchestrator::{LocalWorkspace, RolesPort};
use kevin_router::{
    CatalogRepo, ModelCatalog, PgRouteScoreRepo, Router, SelectRouteQuery as RouterQuery,
};
use kevin_store::migrate::{MigratePolicy, migrate};
use kevin_store::{
    AppendResult, CommandLog, Db, EventStore, NewEvent, PgEventStore, PgPool, StoreError,
    StoredEvent, StreamId,
};
use kevin_worker::SandboxPolicy;
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{ExitError, exit};

/// How a [`Backend`] fans committed events out.
///
/// One-shot commands and `kevin run` own their process, so a
/// `tokio::sync::broadcast` bus is enough. `kevin serve` is a *daemon*: a
/// `kevin runs follow`, a TUI or a second replica running against the same
/// database must see its events, which needs the cross-process bus. Both read
/// events back from the event store by global position ([`PgEventStore`]
/// implements `kevin_bus::EventSource`), so `NOTIFY` is only a wake-up and no
/// event can be missed if one is lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusMode {
    /// `InProcBus`: broadcast fan-out inside this process only.
    InProcess,
    /// `PgNotifyBus`: Postgres `LISTEN/NOTIFY` wake-ups + catch-up from the store.
    CrossProcess,
}

impl BusMode {
    async fn build(
        self,
        pool: &PgPool,
        store: &Arc<PgEventStore>,
    ) -> anyhow::Result<(Arc<dyn EventBus>, Option<PgNotifyBus>)> {
        match self {
            BusMode::InProcess => Ok((
                Arc::new(InProcBus::with_defaults()) as Arc<dyn EventBus>,
                None,
            )),
            BusMode::CrossProcess => {
                let source = Arc::clone(store) as Arc<dyn kevin_bus::EventSource>;
                let bus = PgNotifyBus::new(pool.clone(), source)
                    .await
                    .map_err(|e| ExitError::new(exit::UNREACHABLE, format!("event bus: {e}")))?;
                Ok((Arc::new(bus.clone()) as Arc<dyn EventBus>, Some(bus)))
            }
        }
    }
}

/// Shared plumbing every embedded command needs: pool, store, bus, services
/// and read models.
pub struct Backend {
    config: Arc<KevinConfig>,
    pool: PgPool,
    store: Arc<PgEventStore>,
    store_erased: Arc<dyn EventStore>,
    bus: Arc<dyn EventBus>,
    bus_mode: BusMode,
    /// The cross-process bus, kept concrete so [`Backend::close`] can stop its
    /// listener before the pool is closed.
    pg_bus: Option<PgNotifyBus>,
    commands: Arc<CommandLog>,
    ids: Arc<UuidV7IdGen>,
    clock: Arc<SystemClock>,
    read: ReadModels,
    runs: RunService,
    tasks: TaskService,
    questions: QuestionService,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backend")
            .field("profile", &self.config.kevin.profile)
            .finish_non_exhaustive()
    }
}

impl Backend {
    /// Connects to Postgres, applies the migration policy of
    /// `database.auto_migrate` and builds the services and read models with an
    /// in-process bus ([`BusMode::InProcess`]).
    pub async fn open(config: Arc<KevinConfig>) -> anyhow::Result<Self> {
        Self::open_with(config, BusMode::InProcess).await
    }

    /// [`Backend::open`] with an explicit [`BusMode`]. `kevin serve` opens with
    /// [`BusMode::CrossProcess`] so a second process (a `kevin runs follow`, a
    /// TUI, another daemon replica) sees this instance's events.
    pub async fn open_with(config: Arc<KevinConfig>, bus_mode: BusMode) -> anyhow::Result<Self> {
        // Before anything can log, append an event or write a transcript, the
        // secrets this configuration resolved to are handed to the redaction
        // layer (`plan/09-security.md` §Redaction).
        crate::secrets::register(&config);
        let pool = Db::connect(&config.database)
            .await
            .map_err(|e| ExitError::new(exit::UNREACHABLE, format!("database: {e}")))?;
        let policy = if config.database.auto_migrate {
            MigratePolicy::Apply
        } else {
            MigratePolicy::CheckOnly
        };
        migrate(&pool, policy)
            .await
            .map_err(|e| ExitError::new(exit::UNREACHABLE, format!("migrations: {e}")))?;

        let store = Arc::new(PgEventStore::new(pool.clone()));
        let (bus, pg_bus) = bus_mode.build(&pool, &store).await?;
        let commands = Arc::new(CommandLog::new(pool.clone()));
        let ids = Arc::new(UuidV7IdGen);
        let clock = Arc::new(SystemClock);
        let core = ServiceCore::new(
            Arc::clone(&store) as Arc<dyn EventStore>,
            Arc::clone(&bus),
            Arc::clone(&commands) as Arc<dyn kevin_orchestrator::ports::CommandIdempotency>,
            Arc::clone(&clock) as Arc<dyn Clock>,
            Arc::clone(&ids) as Arc<dyn IdGen>,
        );
        Ok(Self {
            read: ReadModels::new(pool.clone()),
            runs: RunService::new(core.clone()),
            tasks: TaskService::new(core.clone()),
            questions: QuestionService::new(core),
            config,
            pool,
            store_erased: Arc::clone(&store) as Arc<dyn EventStore>,
            store,
            bus,
            bus_mode,
            pg_bus,
            commands,
            ids,
            clock,
        })
    }

    /// The effective configuration.
    #[must_use]
    pub fn config(&self) -> &Arc<KevinConfig> {
        &self.config
    }

    /// The connection pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The event store.
    #[must_use]
    pub fn store(&self) -> &Arc<PgEventStore> {
        &self.store
    }

    /// An event-store handle that also publishes what it appends, for the
    /// crates that write their own aggregate (`kevin-evaluator`,
    /// `kevin-memory`).
    #[must_use]
    pub fn publishing_store(&self) -> Arc<dyn EventStore> {
        Arc::new(PublishingStore {
            inner: Arc::clone(&self.store) as Arc<dyn EventStore>,
            bus: Arc::clone(&self.bus),
        })
    }

    /// How this backend fans events out.
    #[must_use]
    pub const fn bus_mode(&self) -> BusMode {
        self.bus_mode
    }

    /// The event store behind its trait object (what `kevin-api` wants).
    #[must_use]
    pub fn store_erased(&self) -> &Arc<dyn EventStore> {
        &self.store_erased
    }

    /// The event bus behind its trait object (what `kevin-api` wants).
    #[must_use]
    pub const fn bus_erased(&self) -> &Arc<dyn EventBus> {
        &self.bus
    }

    /// Typed queries over the `orch.*` read models.
    #[must_use]
    pub const fn read_models(&self) -> &ReadModels {
        &self.read
    }

    /// `Run` commands.
    #[must_use]
    pub const fn run_service(&self) -> &RunService {
        &self.runs
    }

    /// `Task` commands.
    #[must_use]
    pub const fn task_service(&self) -> &TaskService {
        &self.tasks
    }

    /// `Question` commands.
    #[must_use]
    pub const fn question_service(&self) -> &QuestionService {
        &self.questions
    }

    /// The command idempotency log (`core.processed_commands`), erased.
    #[must_use]
    pub fn commands_erased(&self) -> Arc<dyn kevin_orchestrator::ports::CommandIdempotency> {
        Arc::clone(&self.commands) as Arc<dyn kevin_orchestrator::ports::CommandIdempotency>
    }

    /// The clock, erased.
    #[must_use]
    pub fn clock_erased(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock) as Arc<dyn Clock>
    }

    /// The id generator, erased.
    #[must_use]
    pub fn ids_erased(&self) -> Arc<dyn IdGen> {
        Arc::clone(&self.ids) as Arc<dyn IdGen>
    }

    /// Deterministic id generator.
    #[must_use]
    pub fn ids(&self) -> &Arc<UuidV7IdGen> {
        &self.ids
    }

    /// Brings every read model up to the store head, in registry order.
    ///
    /// One-shot commands (`kevin answer`, `kevin approve`, …) append events in
    /// a process that runs no projection follower; without this the row they
    /// just changed would still show the previous state.
    pub async fn catch_up(&self) -> anyhow::Result<()> {
        let store: Arc<dyn EventStore> = Arc::clone(&self.store) as Arc<dyn EventStore>;
        for projection in projections::all() {
            let mut runner =
                ProjectionRunner::new(projection, self.pool.clone(), Arc::clone(&store));
            runner.load_checkpoint().await?;
            runner.catch_up().await?;
        }
        Ok(())
    }

    /// Stops the bus and closes the pool.
    ///
    /// Order matters: a `PgNotifyBus` holds a `LISTEN` connection for as long
    /// as its pump runs, and `PgPool::close()` waits for every connection to
    /// come back — closing the pool first hangs the shutdown forever.
    pub async fn close(self) {
        if let Some(bus) = &self.pg_bus {
            bus.shutdown();
        }
        self.pool.close().await;
    }
}

/// A booted, in-process Kevin: [`Backend`] + projection runners + orchestrator.
pub struct EmbeddedRuntime {
    backend: Backend,
    handle: Arc<Handle>,
    memory: Option<Arc<MemoryStore>>,
    cancel: CancellationToken,
    projections: Vec<JoinHandle<Result<(), projections::ProjectionError>>>,
}

impl std::fmt::Debug for EmbeddedRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedRuntime")
            .field("projections", &self.projections.len())
            .finish_non_exhaustive()
    }
}

impl EmbeddedRuntime {
    /// Boots the runtime for the current directory.
    pub async fn start(config: Arc<KevinConfig>) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir()
            .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("current directory: {e}")))?;
        Self::start_in(config, &cwd).await
    }

    /// Boots the runtime with `repo_root` as the repository task workspaces are
    /// derived from.
    pub async fn start_in(config: Arc<KevinConfig>, repo_root: &Path) -> anyhow::Result<Self> {
        let backend = Backend::open(Arc::clone(&config)).await?;
        Self::boot_on(backend, repo_root, Vec::new()).await
    }

    /// [`EmbeddedRuntime::start_in`] on a [`Backend`] the caller already
    /// opened, with extra platform briefings.
    ///
    /// The seam exists for `kevin serve --kohral`: the Kohral contract requires
    /// the `runtime_restarted` sweep to run **before** the supervisor rebuilds
    /// actors (`plan/08-kohral-runtime.md` §1.9), and the briefing provider to
    /// be in place before the first role call (§5.1). Both need an open
    /// database and neither belongs in this crate.
    pub async fn boot_on(
        backend: Backend,
        repo_root: &Path,
        system_context: Vec<Arc<dyn SystemContextProvider>>,
    ) -> anyhow::Result<Self> {
        let config = Arc::clone(backend.config());
        let cancel = CancellationToken::new();

        let workers = Arc::new(
            WorkerRegistry::from_config(
                &RegistryConfig::from(&*config),
                SandboxPolicy::from(&config.sandbox),
            )
            .map_err(|errors| ExitError::new(exit::INVALID_ARGS, format!("workers: {errors}")))?,
        );
        let workspace = Arc::new(
            LocalWorkspace::new(repo_root, &config)
                .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("workspace: {e}")))?,
        );
        let router = Arc::new(build_router(&backend, &config).await?);
        let roles = Arc::new(RoleRunnerRoles::new(
            Arc::clone(&workers),
            Arc::clone(&config),
        )) as Arc<dyn RolesPort>;
        let memory_store = build_memory_store(&backend, &config).await;
        let memory = memory_store.clone().map(|store| {
            Arc::new(PgMemory {
                store,
                max_tokens: MemoryCfg::from_config(&config).context_max_tokens,
            }) as Arc<dyn MemoryPort>
        });
        let evaluator = build_evaluator(
            &backend,
            &config,
            &workers,
            repo_root,
            &router,
            memory_store.clone(),
        );
        let task_log = Arc::new(TaskLog::new(backend.pool.clone()));

        let store: Arc<dyn EventStore> = Arc::clone(&backend.store) as Arc<dyn EventStore>;
        let bus: Arc<dyn EventBus> = Arc::clone(&backend.bus);
        let projections = projections::spawn_all(&backend.pool, &store, &bus, &cancel);

        let handle = Orchestrator::boot(Deps {
            store,
            bus,
            commands: Arc::clone(&backend.commands)
                as Arc<dyn kevin_orchestrator::ports::CommandIdempotency>,
            workers,
            workspace,
            router: router as Arc<dyn RouterPort>,
            roles,
            memory,
            evaluator,
            config: Arc::clone(&config),
            clock: Arc::clone(&backend.clock) as Arc<dyn Clock>,
            ids: Arc::clone(&backend.ids) as Arc<dyn IdGen>,
            system_context,
            task_log: Some(task_log),
            tick_interval: DEFAULT_TICK,
        })
        .await
        .map_err(|e| ExitError::new(exit::FAILED, format!("orchestrator: {e}")))?;

        Ok(Self {
            backend,
            handle: Arc::new(handle),
            memory: memory_store,
            cancel,
            projections,
        })
    }

    /// The shared plumbing (services, read models, store, bus).
    #[must_use]
    pub const fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The live orchestration engine.
    #[must_use]
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// The engine as a shared handle (`kevin-api`'s `OrchestratorRuntime`).
    #[must_use]
    pub fn handle_arc(&self) -> Arc<Handle> {
        Arc::clone(&self.handle)
    }

    /// The pgvector memory store, when `memory.enabled` and the embedder
    /// loaded — shared with the saga and the evaluator, so `kevin serve` loads
    /// the embedding model exactly once.
    #[must_use]
    pub const fn memory_store(&self) -> Option<&Arc<MemoryStore>> {
        self.memory.as_ref()
    }

    /// Token every background task of this runtime stops on.
    #[must_use]
    pub const fn cancel(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Typed queries over the `orch.*` read models.
    #[must_use]
    pub const fn read_models(&self) -> &ReadModels {
        self.backend.read_models()
    }

    /// Drains the engine, stops the projection runners and closes the pool.
    pub async fn shutdown(self) {
        self.handle.shutdown().await;
        self.cancel.cancel();
        for task in self.projections {
            let _ = task.await;
        }
        self.backend.close().await;
    }
}

/// Materialises `[models]`, snapshots it into `routing.model_aliases` and wraps
/// [`Router`] in the saga's [`RouterPort`].
async fn build_router(backend: &Backend, config: &Arc<KevinConfig>) -> anyhow::Result<PgRouter> {
    let catalog = Arc::new(ModelCatalog::from_config(config));
    if let Err(err) = CatalogRepo::new(backend.pool.clone()).sync(&catalog).await {
        tracing::warn!(error = %err, "model catalog sync failed; routing uses the in-memory catalog");
    }
    let scores = Arc::new(PgRouteScoreRepo::new(backend.pool.clone()));
    Ok(PgRouter {
        router: Arc::new(Router::new(catalog, config, scores)),
        clock: Arc::clone(&backend.clock) as Arc<dyn Clock>,
    })
}

/// Opens the pgvector memory when `memory.enabled`; a failing embedder degrades
/// to "no memory" rather than failing the run. The store is shared by the
/// saga's [`MemoryPort`] and by the evaluator's lesson auto-apply.
async fn build_memory_store(
    backend: &Backend,
    config: &Arc<KevinConfig>,
) -> Option<Arc<MemoryStore>> {
    if !config.memory.enabled {
        return None;
    }
    let cfg = MemoryCfg::from_config(config);
    let embedder = match embed::embedder_from_cfg(&cfg).await {
        Ok(embedder) => embedder,
        Err(err) => {
            tracing::warn!(error = %err, "memory embedder unavailable; memory is disabled for this run");
            return None;
        }
    };
    Some(Arc::new(
        MemoryStore::new(backend.pool.clone(), embedder, cfg)
            .with_events(backend.publishing_store()),
    ))
}

/// The judge (`kevin-evaluator`, WS-19) behind the saga's [`EvaluatorPort`].
///
/// Auto-apply is wired to the same router and memory the saga uses, so an
/// evaluation updates route scores and lessons in place
/// (`plan/06-memory-and-learning.md` §3.4).
fn build_evaluator(
    backend: &Backend,
    config: &Arc<KevinConfig>,
    workers: &Arc<WorkerRegistry>,
    repo_root: &Path,
    router: &Arc<PgRouter>,
    memory: Option<Arc<MemoryStore>>,
) -> Option<Arc<dyn EvaluatorPort>> {
    if !config.evaluation.enabled {
        return None;
    }
    let mut auto = AutoApply::new(config.evaluation.auto_apply.iter().copied())
        .with_router(Arc::clone(&router.router) as Arc<dyn kevin_evaluator::RouterPort>);
    if let Some(store) = memory {
        auto = auto.with_memory(store as Arc<dyn kevin_evaluator::MemoryPort>);
    }
    let repo = Arc::new(PgEvaluationRepo::new(
        backend.pool.clone(),
        backend.publishing_store(),
    ));
    let evaluator = Evaluator::new(
        EvaluatorConfig::from_config(config),
        Arc::clone(workers),
        kevin_worker::Workspace::in_place(repo_root.to_path_buf()),
        repo,
        auto,
    );
    Some(Arc::new(EvaluationRunner::new(
        Arc::new(evaluator),
        backend.read_models().clone(),
    )))
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// [`RouterPort`] over [`kevin_router::Router`] (WS-09).
struct PgRouter {
    router: Arc<Router>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgRouter").finish_non_exhaustive()
    }
}

#[async_trait]
impl RouterPort for PgRouter {
    async fn select(&self, query: SelectRouteQuery) -> PortResult<RouteSelection> {
        let mut router_query = RouterQuery::new(query.kind)
            .complexity(query.complexity)
            .tags(query.tags)
            .exclude(query.exclude);
        router_query.budget_left_usd = query.budget_left_usd;
        router_query.rng_seed = query.rng_seed;
        let selection = self
            .router
            .select(router_query)
            .await
            .map_err(|e| PortError::permanent("router", e.to_string()))?;
        Ok(RouteSelection {
            route: selection.route,
            policy: policy_to_domain(selection.policy),
            candidates: selection
                .candidates
                .into_iter()
                .map(|c| CandidateScore {
                    alias: c.alias,
                    sampled_success: c.sampled_success,
                    quality: c.quality,
                    norm_cost: c.norm_cost,
                    norm_latency: c.norm_latency,
                    score: c.score,
                    samples: c.samples,
                    excluded_reason: c.excluded_reason,
                })
                .collect(),
            catalog_version: selection.catalog_version,
        })
    }

    async fn record_outcome(&self, outcome: RecordRouteOutcome) -> PortResult<()> {
        let attempt = kevin_router::AttemptRef {
            run_id: outcome.run_id.into(),
            task_id: outcome.task_id.into(),
            attempt_id: outcome.attempt_id.into(),
        };
        let cmd = DomainRouteOutcome {
            task_kind: outcome.task_kind,
            alias: outcome.alias,
            success: outcome.success,
            quality: outcome.quality,
            cost_usd: outcome.cost_usd,
            wall_ms: outcome.wall_ms,
            failure_class: outcome.failure_class,
            recorded_at: self.clock.now(),
            prior: kevin_router::BetaPrior::default(),
        };
        self.router
            .record_attempt_outcome(cmd, Some(attempt))
            .await
            .map(|_| ())
            .map_err(|e| PortError::transient("router", e.to_string()))
    }
}

const fn policy_to_domain(policy: kevin_router::Policy) -> kevin_domain::task::RoutingPolicy {
    use kevin_domain::task::RoutingPolicy as Domain;
    match policy {
        kevin_router::Policy::Thompson => Domain::Thompson,
        kevin_router::Policy::EpsilonGreedy => Domain::EpsilonGreedy,
        kevin_router::Policy::Fixed => Domain::Fixed,
        kevin_router::Policy::Fallback => Domain::Fallback,
    }
}

/// [`MemoryPort`] over [`MemoryStore`] / [`ContextBuilder`] (WS-18).
struct PgMemory {
    store: Arc<MemoryStore>,
    max_tokens: usize,
}

impl std::fmt::Debug for PgMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMemory").finish_non_exhaustive()
    }
}

#[async_trait]
impl MemoryPort for PgMemory {
    async fn context_for_intake(
        &self,
        goal: &Goal,
        repo: Option<&str>,
    ) -> PortResult<Option<String>> {
        // The saga passes the repository *name*; the stable scope key is the
        // canonical path, which is what `kevin memory` uses too.
        let _ = repo;
        let repo_id = RepoId::from_path(&goal.cwd);
        let block = ContextBuilder::new(self.store.as_ref())
            .with_max_tokens(self.max_tokens)
            .for_intake(&goal.text, Some(&repo_id))
            .await
            .map_err(|e| PortError::transient("memory", e.to_string()))?;
        Ok((!block.is_empty()).then_some(block.text))
    }

    async fn store_lesson(&self, lesson: Lesson) -> PortResult<()> {
        let request = StoreRequest::lesson(lesson.content).with_tags(lesson.tags);
        self.store
            .store(request)
            .await
            .map(|_| ())
            .map_err(|e| PortError::transient("memory", e.to_string()))
    }
}

/// An [`EventStore`] that fans out what it appends to the bus.
///
/// The orchestrator's services publish their own appends; components that write
/// their aggregate directly through an `EventStore` handle — `kevin-evaluator`
/// (`evaluation.*`) and `kevin-memory` (`memory.*`) — do not, so in the
/// single-process embedded runtime their events would only reach subscribers
/// through a projection catch-up. Wrapping the handle they are given closes
/// that seam without either crate learning about the bus.
struct PublishingStore {
    inner: Arc<dyn EventStore>,
    bus: Arc<dyn EventBus>,
}

impl std::fmt::Debug for PublishingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishingStore").finish_non_exhaustive()
    }
}

#[async_trait]
impl EventStore for PublishingStore {
    async fn append(
        &self,
        stream: &StreamId,
        expected_version: u64,
        events: &[NewEvent],
    ) -> Result<AppendResult, StoreError> {
        let result = self.inner.append(stream, expected_version, events).await?;
        let envelopes: Vec<kevin_bus::Event> =
            result.events.iter().map(|e| e.envelope.clone()).collect();
        if let Err(err) = self.bus.publish(&envelopes).await {
            tracing::warn!(error = %err, "bus publish failed; consumers catch up from the store");
        }
        Ok(result)
    }

    async fn load_stream(
        &self,
        stream: &StreamId,
        from_version: u64,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.inner.load_stream(stream, from_version).await
    }

    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        self.inner.read_all(from_position, limit).await
    }

    fn subscribe_positions(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.subscribe_positions()
    }
}

// ---------------------------------------------------------------------------
// Session helpers shared by the run-facing subcommands
// ---------------------------------------------------------------------------

/// Resolves the effective configuration, mapping config errors to exit code 3.
pub fn resolve(ctx: &crate::Ctx) -> anyhow::Result<KevinConfig> {
    let resolved = crate::cmd::config::load_from_ctx(ctx)
        .map_err(|errors| ExitError::new(exit::INVALID_ARGS, format!("configuration: {errors}")))?;
    Ok(resolved.config)
}

/// Resolves the configuration and refuses server mode.
///
/// `--server` is checked *before* the configuration is loaded: pointing the
/// CLI at a remote Kevin must fail the same way whatever the local files say.
pub fn resolve_embedded(ctx: &crate::Ctx) -> anyhow::Result<KevinConfig> {
    if let Some(url) = ctx
        .global
        .server
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Err(server_mode_unsupported(url));
    }
    let config = resolve(ctx)?;
    if let Some(url) = server_url(ctx, &config) {
        return Err(server_mode_unsupported(&url));
    }
    Ok(config)
}

/// The configured server URL, if any (`--server` > `KEVIN__CLIENT__SERVER_URL`
/// > `client.server_url`); `None` means embedded mode.
#[must_use]
pub fn server_url(ctx: &crate::Ctx, config: &KevinConfig) -> Option<String> {
    ctx.global
        .server
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(config.client.server_url.clone()).filter(|s| !s.trim().is_empty()))
}

/// Fails with the "server mode is not wired yet" error.
///
/// The typed `KevinClient` lands with WS-16 (`kevin-api`) and `kevin serve`
/// with WS-20; until then every subcommand runs embedded.
pub fn server_mode_unsupported(url: &str) -> anyhow::Error {
    ExitError::new(
        exit::NOT_IMPLEMENTED,
        format!(
            "--server {url}: talking to a remote Kevin is not implemented yet (WS-16/WS-20); \
             unset `client.server_url` to run embedded"
        ),
    )
    .into()
}

/// Opens a [`Backend`] in embedded mode, refusing server mode.
pub async fn open_backend(ctx: &crate::Ctx) -> anyhow::Result<Backend> {
    Backend::open(Arc::new(resolve_embedded(ctx)?)).await
}

// ---------------------------------------------------------------------------
// The proposals inbox behind the HTTP API
// ---------------------------------------------------------------------------

/// [`kevin_api::port::EvaluatorPort`] over [`kevin_evaluator::Proposals`].
///
/// `GET /api/v1/proposals` and the accept/reject verbs are part of the API
/// surface (`plan/07-api-and-tui.md` §Endpoints) and back the TUI's
/// "Lessons & proposals" screen. Without this adapter `AppState` has no
/// evaluator port and all three answer `runtime_unavailable`, so the inbox is
/// reachable only from the CLI.
///
/// The adapter lives here rather than in `kevin-api` because the inbox is a
/// `kevin-evaluator` type and `kevin-api` must not depend on it
/// (`plan/01-architecture.md` §Dependency direction); `kevin-cli` is the crate
/// that wires everything.
pub struct InboxEvaluator {
    inbox: Arc<kevin_evaluator::Proposals>,
}

impl std::fmt::Debug for InboxEvaluator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxEvaluator").finish_non_exhaustive()
    }
}

impl InboxEvaluator {
    /// Wraps a proposals inbox.
    #[must_use]
    pub const fn new(inbox: Arc<kevin_evaluator::Proposals>) -> Self {
        Self { inbox }
    }
}

fn proposal_dto(row: &kevin_evaluator::ProposalRow) -> kevin_api::dto::ProposalDto {
    kevin_api::dto::ProposalDto {
        id: row.id,
        evaluation_id: row.evaluation_id,
        kind: kevin_evaluator::repo::kind_str(row.kind).to_owned(),
        body: row.body.clone(),
        status: kevin_evaluator::repo::status_str(row.status).to_owned(),
        created_at: row.created_at,
    }
}

#[async_trait]
impl kevin_api::port::EvaluatorPort for InboxEvaluator {
    async fn proposals(
        &self,
        query: &kevin_api::dto::ProposalsQuery,
    ) -> kevin_api::port::PortResult<kevin_api::dto::Page<kevin_api::dto::ProposalDto>> {
        let status = query
            .status
            .as_deref()
            .map(kevin_evaluator::repo::parse_status)
            .transpose()
            .map_err(|e| kevin_api::port::RuntimeError::Internal(e.to_string()))?;
        let rows = self
            .inbox
            .list(status, query.limit.unwrap_or(50))
            .await
            .map_err(|e| kevin_api::port::RuntimeError::Storage(e.to_string()))?;
        Ok(kevin_api::dto::Page::new(
            rows.iter().map(proposal_dto).collect(),
        ))
    }

    async fn decide_proposal(
        &self,
        proposal_id: kevin_domain::ProposalId,
        accept: bool,
        note: Option<String>,
        ctx: kevin_api::port::CommandCtx,
    ) -> kevin_api::port::PortResult<kevin_api::dto::ProposalDto> {
        let by = match &ctx.actor {
            kevin_domain::Actor::User { name } => name.clone(),
            kevin_domain::Actor::System { component } => component.clone(),
            kevin_domain::Actor::Worker { kind } => kind.to_string(),
            kevin_domain::Actor::Kohral { agent_id } => agent_id.clone(),
        };
        let row = if accept {
            self.inbox
                .accept(proposal_id, &by, note)
                .await
                .map(|outcome| outcome.proposal)
        } else {
            self.inbox.reject(proposal_id, &by, note).await
        };
        match row {
            Ok(row) => Ok(proposal_dto(&row)),
            Err(kevin_evaluator::EvaluatorError::ProposalNotFound(id)) => {
                Err(kevin_api::port::RuntimeError::ProposalNotFound(id))
            }
            Err(err) => Err(kevin_api::port::RuntimeError::Storage(err.to_string())),
        }
    }
}
