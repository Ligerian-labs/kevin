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

use crate::embedded::EmbeddedRuntime;
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
    if args.kohral {
        // The Kohral listener, ledger and conformance wrapper are WS-22
        // (`plan/08-kohral-runtime.md`). Failing here is better than binding a
        // plain Kevin API on `kohral.bind` and letting Kohral talk to it.
        return Err(ExitError::new(
            exit::NOT_IMPLEMENTED,
            "`kevin serve --kohral` is not implemented yet (WS-22): the Kohral runtime \
             contract lands with the `kevin-kohral` crate",
        )
        .into());
    }

    // ---- 1. configuration ---------------------------------------------------
    let resolved = Arc::new(load(ctx, args.bind.as_deref())?);
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
    let runtime = EmbeddedRuntime::start(Arc::clone(&config)).await?;
    let observers = crate::observability::spawn(
        runtime.backend().bus_erased(),
        runtime.backend().pool().clone(),
        runtime.handle_arc(),
        runtime.cancel(),
    );

    // ---- 9. bind the API and flip ready ------------------------------------
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

    tracing::info!(
        { fields::EVENT } = events::startup::READY,
        version = crate::VERSION,
        bind = %local,
        docs = config.server.docs,
        "kevin is ready"
    );
    if !ctx.global.quiet {
        // The API line comes last: it is the marker "the daemon is ready".
        if let Some(metrics) = guard.metrics_addr() {
            println!("metrics on http://{metrics}/metrics");
        }
        println!("kevin serve listening on http://{local}");
    }

    // ---- shutdown -----------------------------------------------------------
    let reloader = spawn_token_reloader(auth);
    signals.terminate().await;
    let forced = shutdown(&runtime, &mut signals, &config).await;
    reloader.abort();

    api_stop.cancel();
    let _ = tokio::time::timeout(FLUSH_BUDGET, server).await;
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
