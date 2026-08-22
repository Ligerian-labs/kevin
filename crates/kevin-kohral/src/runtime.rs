//! Booting the Kohral listener: the `runtime_restarted` sweep, the ledger
//! projection and the router (`plan/08-kohral-runtime.md` §1.9 and §2,
//! `plan/10-observability-ops.md` §Startup).
//!
//! The startup order matters and is the reason this is a type and not three
//! loose functions:
//!
//! ```text
//! 1. migrate                       (kevin db migrate / database.auto_migrate)
//! 2. sweep_runtime_restarted       ← BEFORE Orchestrator::boot
//! 3. Orchestrator::boot            (rebuilds actors for non-terminal runs)
//! 4. KohralRuntime::start          (ledger projection)
//! 5. bind kohral.bind with router()
//! ```
//!
//! Step 2 has to happen first. A turn that was mid-flight when the process
//! died is `failed / runtime_restarted` by contract — Kohral retries it as a
//! *new* turn — so the run must be terminal **before** the supervisor gets a
//! chance to rebuild an actor for it and resume the saga. Doing it the other
//! way round is exactly the "automatic replay" the capabilities document
//! promises Kevin does not do.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Router;
use kevin_api::auth::TokenVerifier;
use kevin_bus::EventBus;
use kevin_config::KevinConfig;
use kevin_domain::run::FailRun;
use kevin_domain::{Actor, Clock, CommandId, FailureClass, IdGen, RunFailureReason, RunId};
use kevin_orchestrator::Handle;
use kevin_orchestrator::ports::CommandIdempotency;
use kevin_orchestrator::projections::ProjectionRunner;
use kevin_orchestrator::services::{CommandContext, RunService, ServiceCore};
use kevin_store::{EventStore, PgPool};
use kevin_telemetry::events;
use kevin_worker::WorkerRegistry;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::error::KohralResult;
use crate::ledger::{RUNTIME_RESTARTED_MESSAGE, RunsLedger};
use crate::projection::KohralLedgerProjection;
use crate::state::{KohralOptions, KohralState};

/// Everything the Kohral listener needs on top of a booted orchestrator.
pub struct KohralDeps {
    /// The booted engine.
    pub handle: Arc<Handle>,
    /// The database the ledger and the projection live in.
    pub pool: PgPool,
    /// Event store the projection catches up from.
    pub store: Arc<dyn EventStore>,
    /// Bus the projection follows.
    pub bus: Arc<dyn EventBus>,
    /// Effective configuration.
    pub config: Arc<KevinConfig>,
    /// The Kohral bearer token (`kohral.token_file`).
    pub auth: Arc<TokenVerifier>,
    /// Worker registry, probed once for the model catalog.
    pub workers: Arc<WorkerRegistry>,
    /// Listener options.
    pub options: KohralOptions,
}

impl std::fmt::Debug for KohralDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KohralDeps")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// The live Kohral listener.
#[derive(Debug)]
pub struct KohralRuntime {
    state: KohralState,
    projection: JoinHandle<()>,
    cancel: CancellationToken,
}

impl KohralRuntime {
    /// Starts the ledger projection and builds the listener state.
    ///
    /// Call **after** [`sweep_runtime_restarted`] and `Orchestrator::boot`.
    pub async fn start(deps: KohralDeps) -> KohralResult<Self> {
        let ledger = RunsLedger::new(deps.pool.clone());
        let authenticated = probe(&deps.workers).await;
        tracing::info!(
            authenticated = ?authenticated,
            "Kohral model catalog will offer the authenticated workers"
        );

        let cancel = CancellationToken::new();
        let runner = ProjectionRunner::new(
            Box::new(KohralLedgerProjection::new(deps.options.narrative)),
            deps.pool.clone(),
            Arc::clone(&deps.store),
        );
        let bus = Arc::clone(&deps.bus);
        let token = cancel.clone();
        let projection = tokio::spawn(async move {
            if let Err(error) = runner.run(bus, token).await {
                tracing::error!(error = %error, "the Kohral ledger projection stopped");
            }
        });

        let state = KohralState::new(
            deps.handle,
            ledger,
            deps.auth,
            deps.config,
            authenticated,
            deps.options,
        );
        Ok(Self {
            state,
            projection,
            cancel,
        })
    }

    /// The axum router to mount on `kohral.bind`.
    pub fn router(&self) -> Router {
        crate::routes::router(self.state.clone())
    }

    /// The listener state (tests, `/health/detailed` wiring).
    #[must_use]
    pub fn state(&self) -> &KohralState {
        &self.state
    }

    /// Stops the ledger projection.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        let _ = self.projection.await;
    }

    /// Kills the projection without stopping it cleanly (crash simulation).
    pub fn abort(&self) {
        self.projection.abort();
    }
}

/// Which worker kinds can actually be called right now.
async fn probe(workers: &WorkerRegistry) -> BTreeSet<kevin_domain::WorkerKind> {
    workers
        .doctor_all()
        .await
        .into_iter()
        .filter(kevin_worker::Doctor::is_healthy)
        .map(|doctor| doctor.kind)
        .collect()
}

/// Terminalises every turn that was still running when the process died.
///
/// Both halves happen, in this order:
///
/// 1. `run.failed { reason: runtime_restarted, class: RuntimeRestarted }` is
///    appended to each affected run, so the aggregate is terminal and
///    `Orchestrator::boot` will not rebuild an actor (and therefore will not
///    re-issue the role call the turn died in);
/// 2. the ledger row becomes `failed / runtime_restarted` with `seq + 1` and
///    its partial output preserved, which is what Kohral polls.
///
/// The projection would eventually derive (2) from (1), but the ledger update
/// is done here too: a Kohral turn that never reached the orchestrator (killed
/// between the ledger insert and `run.started`) has no run stream to fold.
///
/// Returns the runs it terminalised.
pub async fn sweep_runtime_restarted(
    pool: &PgPool,
    store: Arc<dyn EventStore>,
    bus: Arc<dyn EventBus>,
    commands: Arc<dyn CommandIdempotency>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
    instance_name: &str,
) -> KohralResult<Vec<RunId>> {
    let ledger = RunsLedger::new(pool.clone());
    let runs = RunService::new(ServiceCore::new(store, bus, commands, clock, ids));
    let terminalised = ledger.sweep_runtime_restarted().await?;

    let mut ids = Vec::with_capacity(terminalised.len());
    for uuid in terminalised {
        let run_id = RunId::from_uuid(uuid);
        ids.push(run_id);
        let ctx = CommandContext::new(
            CommandId::new(),
            Actor::kohral(instance_name.to_owned()),
            run_id,
        );
        let cmd = FailRun {
            reason: RunFailureReason::RuntimeRestarted,
            class: FailureClass::RuntimeRestarted,
            message: Some(RUNTIME_RESTARTED_MESSAGE.to_owned()),
        };
        match runs.fail(run_id, cmd, &ctx).await {
            Ok(_) => {}
            // Already terminal (the orchestrator got there first, or the run
            // never started): the ledger row is what Kohral reads, and it is
            // already correct.
            Err(error) if error.is_invalid_transition() => {}
            Err(error) => {
                tracing::warn!(error = %error, run_id = %run_id, "failing a restarted turn");
            }
        }
    }

    if !ids.is_empty() {
        tracing::warn!(
            { kevin_telemetry::fields::EVENT } = events::kohral::RUNTIME_RESTARTED,
            turns = ids.len(),
            "terminalised Kohral turns that did not survive the restart"
        );
        metrics::counter!(
            kevin_telemetry::metrics::KOHRAL_TURNS_TOTAL,
            "outcome" => "runtime_restarted"
        )
        .increment(ids.len() as u64);
    }
    Ok(ids)
}
