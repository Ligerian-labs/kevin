//! `kevin serve` — the daemon (`plan/10-observability-ops.md` §Startup and
//! shutdown, `plan/07-api-and-tui.md` §3).
//!
//! ```text
//! startup   1 load + validate config            (exit 3 with every error)
//!           2 init telemetry + metrics listener (telemetry.metrics_bind)
//!           3 connect Postgres
//!           4 migrations per `database.auto_migrate`
//!           5 terminalise stale attempts as `runtime_restarted`
//!           6 rebuild a RunActor per non-terminal run
//!           7 start projections + the metrics observer
//!           8 the worker registry is built with the runtime
//!           9 bind the API, flip ready, log `kevin.startup.ready`
//!
//! shutdown  1 SIGTERM/SIGINT → unready, stop admitting (`503 draining`)
//!           2 running attempts get `kevin.shutdown_grace_period`
//!           3 after the grace: cancel → the saga records
//!             `task.attempt_failed { class: Transient, "runtime_shutdown" }`
//!           4 stop the listener, flush projections and telemetry
//!           5 close the pool, exit 0 (1 when the second signal forced it)
//! ```
//!
//! Steps 3–8 are [`EmbeddedRuntime`]: the daemon and `kevin run` boot the very
//! same runtime, which is why a restart of either terminalises the attempts the
//! other left behind. `SIGHUP` re-reads `server.auth_token_file` so
//! `kevin config rotate-token` needs no downtime, and `/metrics` is served on
//! `telemetry.metrics_bind` only — never on the API bind (plan/10 §Metrics).
//!
//! With `--kohral` (or `kevin.profile = "kohral"`) a **second** listener is
//! bound on `kohral.bind` serving `kevin-kohral`'s Hermes-dialect contract
//! (`plan/08-kohral-runtime.md` §6). It is a separate listener with a separate
//! token on purpose: the operator API and the platform contract never share
//! credentials. Two extra steps slot into the sequence around the runtime:
//! `kevin_kohral::sweep_runtime_restarted` runs at step 5, *before* the
//! supervisor rebuilds actors, so a turn that did not survive the last restart
//! is terminal before anything can resume it; and the platform briefing is
//! registered as a `SystemContextProvider` before the first role call.

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args as _;
use kevin_api::adapters::{ProjectionReads, RegistryWorkers, RepoRoutes, StoreEvents, StoreMemory};
use kevin_api::auth::TokenVerifier;
use kevin_api::port::{EventsPort, ReadPort, RuntimePort};
use kevin_api::runtime::OrchestratorRuntime;
use kevin_api::state::AppState;
use kevin_config::{KevinConfig, Resolved};
use kevin_router::PgRouteScoreRepo;
use kevin_store::migrate;
use kevin_store::{Db, PgPool};
use kevin_telemetry::{TelemetryConfig, events, fields};
use tokio_util::sync::CancellationToken;

use crate::embedded::{Backend, BusMode, EmbeddedRuntime};
use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "serve";

/// Longest the telemetry pipeline is given to flush at exit (plan/10 §4).
const FLUSH_BUDGET: Duration = Duration::from_secs(5);

/// Arguments of `kevin serve`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Kohral profile: expose the Kohral runtime contract (`kevin.profile = kohral`).
    #[arg(long)]
    pub kohral: bool,
    /// Address to bind the API on (overrides `server.bind`).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
}

/// The `kevin serve` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME))
        .about("Run Kevin as a daemon (HTTP API + orchestrator)")
}

/// Runs `kevin serve`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    // ---- 1. configuration ---------------------------------------------------
    let mut loaded = load(ctx, args.bind.as_deref())?;
    // `--kohral` is shorthand for the profile's `kohral.enabled`; the profile
    // sets it too, so either way in reaches the same flag.
    loaded.config.kohral.enabled |= args.kohral;
    let kohral_enabled = loaded.config.kohral.enabled;
    let resolved = Arc::new(loaded);
    let config = Arc::new(resolved.config.clone());

    // ---- 2. telemetry -------------------------------------------------------
    let telemetry = TelemetryConfig::from_kevin_config(&config);
    let guard = kevin_telemetry::init(&telemetry)
        .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("telemetry: {e}")))?;
    tracing::info!(
        { fields::EVENT } = events::startup::CONFIG_LOADED,
        profile = %config.kevin.profile,
        instance = %config.kevin.instance_name,
        bind = %config.server.bind,
        metrics_bind = guard.metrics_addr().map(|a| a.to_string()).unwrap_or_default(),
        "configuration loaded"
    );

    // Installed before anything long-running, so a SIGTERM during startup is
    // queued for the shutdown sequence instead of killing the process.
    let mut signals = Signals::install()?;

    // ---- 3./4. database and migrations -------------------------------------
    // `Backend::open` applies the policy, but a *pending* migration set with
    // `auto_migrate = false` must not kill the process: plan/10 §Startup step 4
    // keeps it up, unready, so an operator can diagnose it.
    let pool = connect(&config).await?;
    if !config.database.auto_migrate
        && let Some(pending) = pending_migrations(&pool).await
    {
        pool.close().await;
        return unready_for_migrations(&config, &pending).await;
    }
    pool.close().await;

    // ---- 5.–8. runtime ------------------------------------------------------
    let runtime = boot(&config, kohral_enabled).await?;
    let observers = crate::observability::spawn(
        runtime.backend().bus_erased(),
        runtime.backend().pool().clone(),
        runtime.handle_arc(),
        runtime.cancel(),
    );

    // ---- 9. bind the API and flip ready ------------------------------------
    // A non-loopback bind is only allowed once the token file it relies on
    // really exists with mode 0600 (`plan/09-security.md` §API authentication).
    // Checked here rather than at load time: the file may be created between
    // `kevin config show` and `kevin serve`, but never after the port is open.
    kevin_config::token::check_bind_security(&config)
        .map_err(|e| ExitError::new(exit::INVALID_ARGS, e.to_string()))?;
    let state = app_state(&runtime, &resolved)?;
    let auth = Arc::clone(state.auth());
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .map_err(|e| {
            ExitError::new(
                exit::UNREACHABLE,
                format!("cannot bind {}: {e}", config.server.bind),
            )
        })?;
    let local = listener.local_addr().unwrap_or(config.server.bind);
    let api_stop = CancellationToken::new();
    let server = {
        let api_stop = api_stop.clone();
        let app = kevin_api::router(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move { api_stop.cancelled().await })
            .await
        })
    };

    // ---- 9b. the Kohral contract listener -----------------------------------
    let kohral = if kohral_enabled {
        Some(serve_kohral(&runtime, &config).await?)
    } else {
        None
    };

    tracing::info!(
        { fields::EVENT } = events::startup::READY,
        version = crate::VERSION,
        bind = %local,
        kohral = kohral.as_ref().map(|k| k.address.to_string()).unwrap_or_default(),
        docs = config.server.docs,
        "kevin is ready"
    );
    if !ctx.global.quiet {
        if let Some(metrics) = guard.metrics_addr() {
            println!("metrics on http://{metrics}/metrics");
        }
        if let Some(kohral) = &kohral {
            println!("kohral runtime contract on http://{}", kohral.address);
        }
        // The API line comes last: it is the marker "the daemon is ready".
        println!("kevin serve listening on http://{local}");
    }

    // ---- shutdown -----------------------------------------------------------
    let reloader = spawn_token_reloader(auth);
    signals.terminate().await;
    let forced = shutdown(&runtime, &mut signals, &config).await;
    reloader.abort();

    api_stop.cancel();
    let _ = tokio::time::timeout(FLUSH_BUDGET, server).await;
    if let Some(kohral) = kohral {
        kohral.stop.cancel();
        let _ = tokio::time::timeout(FLUSH_BUDGET, kohral.server).await;
        kohral.runtime.shutdown().await;
    }
    for observer in observers {
        observer.abort();
    }
    runtime.shutdown().await;

    tracing::info!(
        { fields::EVENT } = if forced {
            events::shutdown::FORCED
        } else {
            events::shutdown::DRAINED
        },
        "kevin stopped"
    );
    drop(guard);
    Ok(if forced {
        ExitCode::from(exit::FAILED)
    } else {
        ExitCode::SUCCESS
    })
}

/// Steps 1–3 of the shutdown sequence; `true` when a second signal forced it.
async fn shutdown(
    runtime: &EmbeddedRuntime,
    signals: &mut Signals,
    config: &KevinConfig,
) -> bool {
    let grace = config.kevin.shutdown_grace_period;
    tracing::info!(
        { fields::EVENT } = events::shutdown::BEGIN,
        grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
        "draining before shutdown"
    );
    // 1. unready + stop admitting. The listener stays up so `/readyz` answers
    //    503 and a load balancer moves off this instance before it disappears.
    runtime.handle().drain().await;
    // 2. running attempts get the grace period; a second signal cuts it short.
    let supervisor = Arc::clone(runtime.handle().supervisor());
    tokio::select! {
        () = supervisor.await_idle(grace) => false,
        () = signals.terminate() => {
            tracing::warn!(
                { fields::EVENT } = events::shutdown::FORCED,
                "second signal: cancelling running attempts now"
            );
            true
        }
    }
}

// ---------------------------------------------------------------------------
// Startup helpers
// ---------------------------------------------------------------------------

/// Steps 5–8, with the two Kohral steps folded in at the right places.
///
/// Ordering is the contract, not a preference: the sweep must see the ledger
/// **before** `Orchestrator::boot` rebuilds a `RunActor` for a run that was
/// mid-turn, or the saga resumes work Kohral was promised would never be
/// replayed (`plan/08-kohral-runtime.md` §1.9, `run_automatic_replay: false`).
async fn boot(config: &Arc<KevinConfig>, kohral: bool) -> anyhow::Result<EmbeddedRuntime> {
    let cwd = std::env::current_dir()
        .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("current directory: {e}")))?;
    // The daemon owns the runtime for *other* processes too, so it fans events
    // out over Postgres `LISTEN/NOTIFY` rather than a process-local broadcast:
    // a `kevin runs follow`, a TUI or a second replica attaches to this
    // instance instead of seeing nothing (`plan/01` §Event-driven core).
    let backend = Backend::open_with(Arc::clone(config), BusMode::CrossProcess).await?;
    if !kohral {
        return EmbeddedRuntime::boot_on(backend, &cwd, Vec::new()).await;
    }

    let restarted = kevin_kohral::sweep_runtime_restarted(
        backend.pool(),
        Arc::clone(backend.store_erased()),
        Arc::clone(backend.bus_erased()),
        backend.commands_erased(),
        backend.clock_erased(),
        backend.ids_erased(),
        &config.kevin.instance_name,
    )
    .await
    .map_err(|e| ExitError::new(exit::FAILED, format!("kohral ledger sweep: {e}")))?;
    if !restarted.is_empty() {
        tracing::warn!(
            { fields::EVENT } = events::kohral::RUNTIME_RESTARTED,
            turns = restarted.len(),
            "terminalised Kohral turns that did not survive the last restart"
        );
    }

    let files = kevin_kohral::briefing::BriefingFiles::from_config(&config.kohral);
    let briefing = kevin_kohral::briefing::provider(&files);
    EmbeddedRuntime::boot_on(backend, &cwd, vec![briefing]).await
}

/// The live Kohral listener.
struct KohralListener {
    runtime: kevin_kohral::KohralRuntime,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    stop: CancellationToken,
    address: SocketAddr,
}

/// Binds `kohral.bind` with the Hermes-dialect router (`plan/08` §1.1).
async fn serve_kohral(
    runtime: &EmbeddedRuntime,
    config: &Arc<KevinConfig>,
) -> anyhow::Result<KohralListener> {
    let token_file = &config.kohral.token_file;
    let auth = TokenVerifier::from_file(token_file, config.server.token_grace).map_err(|e| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!(
                "kohral.token_file {}: {e} (Kohral mounts this secret as \
                 KEVIN_RUNTIME_TOKEN → API_SERVER_KEY)",
                token_file.display()
            ),
        )
    })?;
    let backend = runtime.backend();
    let kohral = kevin_kohral::KohralRuntime::start(kevin_kohral::KohralDeps {
        handle: runtime.handle_arc(),
        pool: backend.pool().clone(),
        store: Arc::clone(backend.store_erased()),
        bus: Arc::clone(backend.bus_erased()),
        config: Arc::clone(config),
        auth: Arc::new(auth),
        workers: Arc::clone(&runtime.handle().deps().workers),
        options: kevin_kohral::KohralOptions::from_config(config, crate::VERSION),
    })
    .await
    .map_err(|e| ExitError::new(exit::FAILED, format!("kohral listener: {e}")))?;

    let listener = tokio::net::TcpListener::bind(config.kohral.bind)
        .await
        .map_err(|e| {
            ExitError::new(
                exit::UNREACHABLE,
                format!("cannot bind kohral.bind {}: {e}", config.kohral.bind),
            )
        })?;
    let address = listener.local_addr().unwrap_or(config.kohral.bind);
    let stop = CancellationToken::new();
    let shutdown = stop.clone();
    let app = kohral.router();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
    });
    Ok(KohralListener {
        runtime: kohral,
        server,
        stop,
        address,
    })
}

/// Loads and validates the configuration, applying `--bind`.
fn load(ctx: &Ctx, bind: Option<&str>) -> anyhow::Result<Resolved> {
    let mut resolved = crate::cmd::config::load_from_ctx(ctx)
        .map_err(|errors| ExitError::new(exit::INVALID_ARGS, format!("configuration: {errors}")))?;
    if let Some(bind) = bind {
        resolved.config.server.bind = bind.parse::<SocketAddr>().map_err(|e| {
            ExitError::new(exit::INVALID_ARGS, format!("--bind {bind}: {e}"))
        })?;
    }
    Ok(resolved)
}

async fn connect(config: &KevinConfig) -> anyhow::Result<PgPool> {
    Db::connect(&config.database)
        .await
        .map_err(|e| ExitError::new(exit::UNREACHABLE, format!("database: {e}")).into())
}

/// Versions the database is missing, or `None` when it is current.
async fn pending_migrations(pool: &PgPool) -> Option<Vec<i64>> {
    let status = migrate::status(pool).await.ok()?;
    let pending = status.pending();
    (!pending.is_empty()).then_some(pending)
}

/// `database.auto_migrate = false` with pending migrations: plan/10 §Startup
/// step 4 keeps the process alive and unready for diagnosis instead of exiting.
/// Only the health surface is served — nothing can accept work without the
/// schema it was compiled against.
async fn unready_for_migrations(config: &KevinConfig, pending: &[i64]) -> anyhow::Result<ExitCode> {
    tracing::error!(
        { fields::EVENT } = events::startup::CONFIG_LOADED,
        pending = ?pending,
        "migrations are pending and database.auto_migrate is false; serving /healthz only \
         (run `kevin db migrate`)"
    );
    let app = axum::Router::new()
        .route(
            "/healthz",
            axum::routing::get(|| async { axum::Json(serde_json::json!({ "status": "ok" })) }),
        )
        .route(
            "/readyz",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(serde_json::json!({
                        "status": "not_ready",
                        "checks": { "migrations": "pending" }
                    })),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind(config.server.bind)
        .await
        .map_err(|e| {
            ExitError::new(
                exit::UNREACHABLE,
                format!("cannot bind {}: {e}", config.server.bind),
            )
        })?;
    let mut signals = Signals::install()?;
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move { signals.terminate().await })
        .await;
    Err(ExitError::new(exit::FAILED, "migrations pending; nothing was served").into())
}

/// Wires every port the HTTP API knows about to its real implementation.
fn app_state(runtime: &EmbeddedRuntime, resolved: &Arc<Resolved>) -> anyhow::Result<AppState> {
    let backend = runtime.backend();
    let config = backend.config();
    let read = backend.read_models().clone();
    let token_file = crate::cmd::config::resolved_token_path(resolved);
    let auth = TokenVerifier::from_file(&token_file, config.server.token_grace).map_err(|e| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!(
                "server.auth_token_file {}: {e} (run `kevin config init`)",
                token_file.display()
            ),
        )
    })?;

    let mut builder = AppState::builder(
        Arc::new(OrchestratorRuntime::new(runtime.handle_arc(), read.clone()))
            as Arc<dyn RuntimePort>,
        Arc::new(ProjectionReads::new(read)) as Arc<dyn ReadPort>,
        Arc::new(StoreEvents::new(
            Arc::clone(backend.store_erased()),
            Arc::clone(backend.bus_erased()),
        )) as Arc<dyn EventsPort>,
        Arc::new(auth),
    )
    .config(Arc::clone(resolved))
    .router_port(Arc::new(RepoRoutes::new(Arc::new(PgRouteScoreRepo::new(
        backend.pool().clone(),
    )))))
    .workers(Arc::new(RegistryWorkers::new(Arc::clone(
        &runtime.handle().deps().workers,
    ))));
    if let Some(memory) = runtime.memory_store() {
        builder = builder.memory(Arc::new(StoreMemory::new(Arc::clone(memory))));
    }
    // The proposals inbox: without it `GET /api/v1/proposals` and the
    // accept/reject verbs answer `runtime_unavailable`, and the TUI's
    // "Lessons & proposals" screen has nothing to show (`plan/07` §Endpoints).
    if config.evaluation.enabled {
        let scores = Arc::new(PgRouteScoreRepo::new(backend.pool().clone()));
        let router = Arc::new(kevin_router::Router::from_config(config, scores));
        let repo = Arc::new(kevin_evaluator::PgEvaluationRepo::new(
            backend.pool().clone(),
            backend.publishing_store(),
        ));
        let inbox = Arc::new(kevin_evaluator::Proposals::new(repo).with_router(router));
        builder = builder.evaluator(Arc::new(crate::embedded::InboxEvaluator::new(inbox)));
    }
    Ok(builder.build())
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// SIGTERM/SIGINT — the shutdown trigger.
pub struct Signals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
}

impl std::fmt::Debug for Signals {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Signals").finish_non_exhaustive()
    }
}

impl Signals {
    /// Installs the handlers.
    pub fn install() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())
                    .map_err(|e| ExitError::new(exit::FAILED, format!("SIGTERM: {e}")))?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Resolves on the next SIGTERM or SIGINT (Ctrl-C).
    pub async fn terminate(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => {},
                result = tokio::signal::ctrl_c() => { let _ = result; },
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }

}

/// Re-reads `server.auth_token_file` on every SIGHUP, so
/// `kevin config rotate-token && systemctl reload kevin` rotates the bearer
/// token with no downtime (the previous one stays valid for
/// `server.token_grace`).
#[cfg(unix)]
fn spawn_token_reloader(auth: Arc<TokenVerifier>) -> tokio::task::JoinHandle<()> {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            tracing::error!("cannot listen for SIGHUP; token rotation needs a restart");
            return;
        };
        while hup.recv().await.is_some() {
            match auth.reload() {
                Ok(()) => tracing::info!(
                    { fields::EVENT } = events::startup::CONFIG_LOADED,
                    "SIGHUP: reloaded the API token"
                ),
                Err(err) => {
                    tracing::error!(error = %err, "SIGHUP: reloading the API token failed");
                }
            }
        }
    })
}

/// No SIGHUP off unix: rotation needs a restart there.
#[cfg(not(unix))]
fn spawn_token_reloader(auth: Arc<TokenVerifier>) -> tokio::task::JoinHandle<()> {
    let _ = auth;
    tokio::spawn(std::future::pending())
}
