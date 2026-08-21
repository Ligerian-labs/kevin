//! [`Orchestrator::boot`] and its [`Handle`] — the startup, drain and shutdown
//! sequences of `plan/10-observability-ops.md` §Startup and shutdown.
//!
//! ```text
//! boot:      services → terminalise stale attempts (`runtime_restarted`)
//!            → rebuild a RunActor per non-terminal run → subscribe the bus
//!            → admit work
//! drain:     stop admitting; actors stop scheduling new attempts; running
//!            attempts keep going
//! shutdown:  drain → `kevin.shutdown_grace_period` → cancel the token tree;
//!            every attempt still running is recorded as
//!            `task.attempt_failed { class: Transient, message: "runtime_shutdown" }`
//! ```

use std::sync::Arc;
use std::time::Duration;

use kevin_bus::{EventBus, SubscriptionFilter};
use kevin_config::KevinConfig;
use kevin_domain::run::StartRun;
use kevin_domain::{Clock, IdGen, RunId};
use kevin_store::EventStore;
use kevin_telemetry::events;
use kevin_worker::WorkerRegistry;
use kevin_worker::usage::{ModelEntryPrices, PriceTable};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::AppError;
use crate::ports::{
    CommandIdempotency, EvaluatorPort, MemoryPort, RolesPort, RouterPort, WorkspacePort,
};
use crate::projections::TaskLog;
use crate::roles::SystemContextProvider;
use crate::run_actor::RunSupervisor;
use crate::scheduler::Bulkheads;
use crate::services::{CommandContext, QuestionService, RunService, ServiceCore, TaskService};

/// Default interval of the saga tick (question expiry, wall-clock budgets,
/// progress-throttle flushes) — `plan/05` §2.
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// Everything the orchestrator needs, as handed in by the process that boots
/// it (`kevin run`, the daemon, the Kohral gateway).
///
/// WS-12 fills `router`, `roles`, `memory` and `evaluator` with the real
/// crates; until then [`crate::testing`] provides in-process fakes.
pub struct Deps {
    /// Event store (`core.events`).
    pub store: Arc<dyn EventStore>,
    /// Event bus the saga listens on.
    pub bus: Arc<dyn EventBus>,
    /// `core.processed_commands` (command idempotency).
    pub commands: Arc<dyn CommandIdempotency>,
    /// Worker adapters.
    pub workers: Arc<WorkerRegistry>,
    /// Per-attempt workspaces and result integration.
    pub workspace: Arc<dyn WorkspacePort>,
    /// Model selection.
    pub router: Arc<dyn RouterPort>,
    /// Kevin's own roles (planner, integrator).
    pub roles: Arc<dyn RolesPort>,
    /// Long-term memory; `None` disables retrieval and lesson storage.
    pub memory: Option<Arc<dyn MemoryPort>>,
    /// Judge; `None` completes runs with `evaluation: skipped`.
    pub evaluator: Option<Arc<dyn EvaluatorPort>>,
    /// Effective configuration.
    pub config: Arc<KevinConfig>,
    /// Clock (tests inject a fake).
    pub clock: Arc<dyn Clock>,
    /// Id generator (tests inject a deterministic one).
    pub ids: Arc<dyn IdGen>,
    /// Platform briefings prepended to role calls (Kohral).
    pub system_context: Vec<Arc<dyn SystemContextProvider>>,
    /// `orch.task_log` writer (WS-11); `None` drops worker log lines.
    pub task_log: Option<Arc<TaskLog>>,
    /// Saga tick interval; [`DEFAULT_TICK`] in production.
    pub tick_interval: Duration,
}

impl std::fmt::Debug for Deps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps")
            .field("workers", &self.workers.kinds())
            .field("memory", &self.memory.is_some())
            .field("evaluator", &self.evaluator.is_some())
            .field("system_context", &self.system_context.len())
            .field("tick_interval", &self.tick_interval)
            .finish_non_exhaustive()
    }
}

/// The resolved dependencies every actor and runner shares.
pub struct OrchestratorDeps {
    /// `Run` commands.
    pub runs: RunService,
    /// `Task` commands.
    pub tasks: TaskService,
    /// `Question` commands.
    pub questions: QuestionService,
    /// Event store (boot scans, saga rehydration).
    pub store: Arc<dyn EventStore>,
    /// Event bus.
    pub bus: Arc<dyn EventBus>,
    /// Worker adapters.
    pub workers: Arc<WorkerRegistry>,
    /// Workspaces and integration.
    pub workspace: Arc<dyn WorkspacePort>,
    /// Model selection.
    pub router: Arc<dyn RouterPort>,
    /// Kevin's own roles.
    pub roles: Arc<dyn RolesPort>,
    /// Long-term memory.
    pub memory: Option<Arc<dyn MemoryPort>>,
    /// Judge.
    pub evaluator: Option<Arc<dyn EvaluatorPort>>,
    /// Effective configuration.
    pub config: Arc<KevinConfig>,
    /// Clock.
    pub clock: Arc<dyn Clock>,
    /// Id generator.
    pub ids: Arc<dyn IdGen>,
    /// Platform briefings.
    pub system_context: Vec<Arc<dyn SystemContextProvider>>,
    /// `orch.task_log` writer (WS-11).
    pub task_log: Option<Arc<TaskLog>>,
    /// Price table used when a worker reports no cost.
    pub prices: Arc<dyn PriceTable>,
    /// Global and per-worker-kind concurrency limits.
    pub bulkheads: Bulkheads,
    /// Saga tick interval.
    pub tick_interval: Duration,
}

impl std::fmt::Debug for OrchestratorDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorDeps")
            .field("bulkheads", &self.bulkheads)
            .field("tick_interval", &self.tick_interval)
            .finish_non_exhaustive()
    }
}

impl OrchestratorDeps {
    /// A fresh system-actor command context for `run_id`.
    #[must_use]
    pub fn ctx(&self, run_id: RunId) -> CommandContext {
        CommandContext::system(self.ids.as_ref(), run_id)
    }
}

/// Boots the orchestration engine.
#[derive(Debug)]
pub struct Orchestrator;

impl Orchestrator {
    /// Runs the startup sequence and returns the live [`Handle`].
    ///
    /// Steps 5–6 of `plan/10` §Startup: stale attempts are terminalised with
    /// `runtime_restarted` **before** any actor is rebuilt, so an attempt that
    /// was running when the process died is never resumed or replayed.
    pub async fn boot(deps: Deps) -> Result<Handle, AppError> {
        let core = ServiceCore::new(
            Arc::clone(&deps.store),
            Arc::clone(&deps.bus),
            Arc::clone(&deps.commands),
            Arc::clone(&deps.clock),
            Arc::clone(&deps.ids),
        );
        let prices: Arc<dyn PriceTable> =
            Arc::new(ModelEntryPrices::new(deps.config.models.clone()));
        let bulkheads = Bulkheads::from_config(&deps.config);
        let resolved = Arc::new(OrchestratorDeps {
            runs: RunService::new(core.clone()),
            tasks: TaskService::new(core.clone()),
            questions: QuestionService::new(core),
            store: deps.store,
            bus: Arc::clone(&deps.bus),
            workers: deps.workers,
            workspace: deps.workspace,
            router: deps.router,
            roles: deps.roles,
            memory: deps.memory,
            evaluator: deps.evaluator,
            config: deps.config,
            clock: deps.clock,
            ids: deps.ids,
            system_context: deps.system_context,
            task_log: deps.task_log,
            prices,
            bulkheads,
            tick_interval: deps.tick_interval,
        });

        let root = CancellationToken::new();
        let supervisor = Arc::new(RunSupervisor::new(Arc::clone(&resolved), root.clone()));

        // Everything published from here on must reach the actors.
        let from_position = deps.bus.position();
        let restarted = supervisor.recover().await?;

        let mut stream = deps.bus.subscribe_from(
            from_position,
            SubscriptionFilter::all().named("orchestrator"),
        );
        let router_supervisor = Arc::clone(&supervisor);
        let router_token = root.clone();
        let subscriber = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = router_token.cancelled() => break,
                    message = stream.next() => match message {
                        Some(kevin_bus::BusMessage::Live(event)) => {
                            router_supervisor.route(event).await;
                        }
                        Some(kevin_bus::BusMessage::Lagged { from, to }) => {
                            tracing::warn!(from, to, "orchestrator bus subscription lagged");
                        }
                        None => break,
                    },
                }
            }
        });

        tracing::info!(
            { kevin_telemetry::fields::EVENT } = events::startup::READY,
            terminalised_attempts = restarted,
            "orchestrator ready"
        );

        Ok(Handle {
            deps: resolved,
            supervisor,
            subscriber,
            root,
        })
    }
}

/// Live orchestration engine.
#[derive(Debug)]
pub struct Handle {
    deps: Arc<OrchestratorDeps>,
    supervisor: Arc<RunSupervisor>,
    subscriber: JoinHandle<()>,
    root: CancellationToken,
}

impl Handle {
    /// The `Run` command service.
    #[must_use]
    pub fn run_service(&self) -> &RunService {
        &self.deps.runs
    }

    /// The `Task` command service.
    #[must_use]
    pub fn task_service(&self) -> &TaskService {
        &self.deps.tasks
    }

    /// The `Question` command service.
    #[must_use]
    pub fn question_service(&self) -> &QuestionService {
        &self.deps.questions
    }

    /// The shared dependencies (services, ports, config).
    #[must_use]
    pub fn deps(&self) -> &Arc<OrchestratorDeps> {
        &self.deps
    }

    /// The run supervisor (actor registry, admission flag).
    #[must_use]
    pub fn supervisor(&self) -> &Arc<RunSupervisor> {
        &self.supervisor
    }

    /// Whether new runs are admitted (`false` while draining).
    #[must_use]
    pub fn is_admitting(&self) -> bool {
        self.supervisor.is_admitting()
    }

    /// Starts a run, refusing while draining (`503 draining`, `plan/10`).
    pub async fn start_run(&self, cmd: StartRun, ctx: &CommandContext) -> Result<RunId, AppError> {
        if !self.supervisor.is_admitting() {
            return Err(AppError::Port(crate::ports::PortError::transient(
                "orchestrator",
                "draining: not admitting new runs",
            )));
        }
        self.deps.runs.start(cmd, ctx).await
    }

    /// Number of live run actors.
    #[must_use]
    pub fn active_runs(&self) -> usize {
        self.supervisor.active_runs()
    }

    /// Stops admitting new runs and tells every actor to stop scheduling new
    /// attempts. Running attempts continue.
    pub async fn drain(&self) {
        tracing::info!(
            { kevin_telemetry::fields::EVENT } = events::shutdown::BEGIN,
            "draining"
        );
        self.supervisor.drain().await;
    }

    /// Kills the engine without recording anything (crash simulation): the
    /// attempts that were running stay non-terminal and the next
    /// [`Orchestrator::boot`] terminalises them as `runtime_restarted`.
    /// The root token is deliberately **not** cancelled: a crashed process
    /// never gets to tell its workers anything, so neither does this.
    pub fn abort(&self) {
        self.supervisor.abort();
        self.subscriber.abort();
    }

    /// Drains, waits `kevin.shutdown_grace_period` for running attempts, then
    /// cancels the token tree and stops every actor.
    pub async fn shutdown(&self) {
        let grace = self.deps.config.kevin.shutdown_grace_period;
        self.supervisor.drain().await;
        self.supervisor.await_idle(grace).await;
        self.supervisor.shutdown().await;
        self.root.cancel();
        self.subscriber.abort();
        tracing::info!(
            { kevin_telemetry::fields::EVENT } = events::shutdown::DRAINED,
            "orchestrator stopped"
        );
    }
}
