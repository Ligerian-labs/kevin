//! Running Kohral's own conformance suite against a real Kevin
//! (`plan/08-kohral-runtime.md` §8, `plan/12-workstreams.md` WS-22).
//!
//! The suite is Kohral's `runtime/conformance/contract.py --runtime hermes`.
//! It is deliberately *not* re-implemented here: a hand-written copy of
//! somebody else's contract drifts the moment they change it. Instead this
//! module
//!
//! - boots a complete Kevin in the **conformance profile** ([`Gateway`]) — real
//!   event store, real orchestrator, real roles, the in-process `fake` worker
//!   carrying the two hooks from `plan/08` §1.9;
//! - locates `contract.py` ([`ContractScript::locate`]) and runs its three
//!   phases against that gateway;
//! - simulates the crash between `accept-crash` and `verify-crash` by aborting
//!   the engine without recording anything ([`Gateway::crash`]) and booting a
//!   fresh one over the same database ([`Gateway::restart`]) — which is what a
//!   `docker kill` does to the container, minus the container.
//!
//! `kevin kohral conformance` is a thin wrapper over exactly this.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use kevin_api::auth::TokenVerifier;
use kevin_bus::InProcBus;
use kevin_config::{Integration, KevinConfig, ModelEntry, WorkspaceCleanup, WorkspaceStrategy};
use kevin_domain::{
    Complexity, ModelAlias, PlanTask, Understanding, UuidV7IdGen, WorkerKind, WorkspacePolicy,
};
use kevin_orchestrator::orchestrator::{Deps, Handle, Orchestrator};
use kevin_orchestrator::ports::{RolesPort, RouterPort};
use kevin_orchestrator::projections::TaskLog;
use kevin_orchestrator::role_port::RoleRunnerRoles;
use kevin_orchestrator::testing::{FixedRouter, TempWorkspaces, fake_route};
use kevin_store::{CommandLog, EventStore, MigratePolicy, PgEventStore, PgPool, migrate};
use kevin_worker::SandboxPolicy;
use kevin_worker::fake::{FakeWorker, KOHRAL_HOLD_INPUT, KOHRAL_REPLY_OUTPUT, Rule, Scenario};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use tokio_util::sync::CancellationToken;

use crate::runtime::{KohralDeps, KohralRuntime, sweep_runtime_restarted};
use crate::state::KohralOptions;

/// The conformance phases of `contract.py`, in the order they must run.
pub const PHASES: [Phase; 3] = [Phase::Basic, Phase::AcceptCrash, Phase::VerifyCrash];

/// One phase of `contract.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Capabilities, catalog, 401, submit/retry/409, terminal `completed`.
    Basic,
    /// Submit a turn that never finishes and stop before it does.
    AcceptCrash,
    /// After the crash: `failed` with `error_code = runtime_restarted`.
    VerifyCrash,
}

impl Phase {
    /// The `phase` argument of `contract.py`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Basic => "basic",
            Phase::AcceptCrash => "accept-crash",
            Phase::VerifyCrash => "verify-crash",
        }
    }
}

// ---------------------------------------------------------------------------
// The fake-worker scenario
// ---------------------------------------------------------------------------

/// The alias every role and task uses in the conformance profile.
pub const ALIAS: &str = "fake";

/// The scenario `plan/08` §1.9 describes, made executable.
///
/// Rule order is the contract:
///
/// 1. `[[KOHRAL_HOLD]]` wins over everything, so the *first* role call of a
///    held turn hangs and the turn stays non-terminal until the crash;
/// 2. Kevin's own roles get structured answers, because a planner that cannot
///    produce `kevin.understanding.v1` fails the run before the worker is ever
///    asked to reply;
/// 3. every other prompt — the plan's tasks — gets the deterministic answer,
///    which is what the integrator then reports as the turn's output.
#[must_use]
pub fn scenario() -> Scenario {
    Scenario::replying(KOHRAL_REPLY_OUTPUT)
        .rule(Rule::matching(KOHRAL_HOLD_INPUT).hold())
        .rule(Rule::matching("planner.understanding").structured(understanding_value()))
        .rule(Rule::matching("planner.plan").structured(plan_value()))
        .rule(Rule::matching("integrator").structured(integration_value()))
}

fn understanding_value() -> serde_json::Value {
    let understanding = Understanding {
        objective: "Answer the operator's message.".to_owned(),
        assumptions: Vec::new(),
        risks: Vec::new(),
        success_criteria: vec!["The operator gets an answer.".to_owned()],
        proposed_questions: Vec::new(),
        complexity: Complexity::Low,
        suggested_task_kinds: vec!["write".to_owned()],
        context_refs: Vec::new(),
    };
    serde_json::to_value(understanding).expect("the understanding fixture serialises")
}

fn plan_value() -> serde_json::Value {
    let plan = kevin_domain::Plan {
        tasks: vec![PlanTask {
            id: "t1".to_owned(),
            title: "Answer".to_owned(),
            kind: "write".to_owned(),
            custom_kind: None,
            instructions: "Reply to the operator.".to_owned(),
            acceptance_criteria: vec!["An answer exists.".to_owned()],
            depends_on: Vec::new(),
            inputs: Vec::new(),
            suggested_tier: None,
            parallel_safe: true,
            workspace_policy: WorkspacePolicy::ReadOnly,
            optional: false,
            allow_push: false,
            output_schema: None,
            suggested_route: None,
        }],
        edges: Vec::new(),
        rationale: "One turn, one answer.".to_owned(),
    };
    serde_json::to_value(plan).expect("the plan fixture serialises")
}

/// The integrator's report. Its `summary` becomes `run.completed.summary`,
/// which the ledger reconciles into `partial_output` — and `contract.py`
/// asserts that value is exactly `kohral-ok`.
fn integration_value() -> serde_json::Value {
    serde_json::json!({
        "status": "integrated",
        "summary": KOHRAL_REPLY_OUTPUT,
        "merged": [],
        "conflicts": [],
        "checks": [],
        "artifacts": [],
    })
}

/// The configuration of the conformance profile: `kevin.profile = "kohral"`
/// with `workers.fake.enabled = true`, everything else off.
#[must_use]
pub fn config(data_dir: &Path) -> KevinConfig {
    let mut config = KevinConfig::default();
    config.kevin.profile = kevin_config::Profile::Kohral;
    config.kevin.data_dir = data_dir.to_path_buf();
    config.kevin.auto_approve_plans = true;
    config.kevin.shutdown_grace_period = Duration::from_millis(200);
    config.kohral.enabled = true;
    config.kohral.run_timeout = Duration::from_secs(120);

    for kind in WorkerKind::ALL {
        let enabled = kind == WorkerKind::Fake;
        match kind {
            WorkerKind::Claude => config.workers.claude.enabled = enabled,
            WorkerKind::Codex => config.workers.codex.enabled = enabled,
            WorkerKind::Pi => config.workers.pi.enabled = enabled,
            WorkerKind::Opencode => config.workers.opencode.enabled = enabled,
            WorkerKind::Fake => config.workers.fake.enabled = enabled,
        }
    }

    let alias = ModelAlias::new(ALIAS).expect("valid alias");
    config.models.clear();
    config
        .models
        .insert(alias.clone(), ModelEntry::new(WorkerKind::Fake, ALIAS));
    config.roles.planner = alias.clone();
    config.roles.clarifier = alias.clone();
    config.roles.judge = alias.clone();
    config.roles.integrator = alias.clone();
    config.roles.default = alias;
    config.roles.effort.clear();

    config.budget.default_run_wall = Duration::from_secs(120);
    config.budget.default_task_wall = Duration::from_secs(20);
    config.orchestrator.role_call_timeout = Duration::from_secs(20);
    config.orchestrator.progress_interval = Duration::from_millis(20);
    config.orchestrator.question_default_timeout = Duration::from_millis(50);

    // There is no repository in a Kohral workload (`plan/05` §5).
    config.workspace.strategy = WorkspaceStrategy::InPlace;
    config.workspace.cleanup = WorkspaceCleanup::Never;
    config.workspace.integration = Integration::None;

    config.concurrency.per_worker_kind.clear();
    config
        .concurrency
        .per_worker_kind
        .insert(WorkerKind::Fake, 8);
    config
}

// ---------------------------------------------------------------------------
// The gateway
// ---------------------------------------------------------------------------

/// A complete Kevin serving the Kohral contract on a loopback port.
pub struct Gateway {
    pool: PgPool,
    store: Arc<PgEventStore>,
    bus: Arc<InProcBus>,
    commands: Arc<CommandLog>,
    workers: Arc<WorkerRegistry>,
    workspaces: Arc<TempWorkspaces>,
    routes: Arc<FixedRouter>,
    task_log: Arc<TaskLog>,
    ids: Arc<UuidV7IdGen>,
    config: Arc<KevinConfig>,
    auth: Arc<TokenVerifier>,
    options: KohralOptions,
    token: String,
    listener: std::net::TcpListener,
    address: std::net::SocketAddr,
    epoch: Option<Epoch>,
    _data: tempfile::TempDir,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("address", &self.address)
            .field("running", &self.epoch.is_some())
            .finish_non_exhaustive()
    }
}

/// One boot of the gateway: engine + listener + serving task.
struct Epoch {
    handle: Arc<Handle>,
    kohral: KohralRuntime,
    server: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

impl Gateway {
    /// Boots a gateway against `pool` (a database the caller owns) in the
    /// conformance profile, i.e. with `partial_output` carrying the answer and
    /// nothing else — which is what `contract.py` asserts on.
    pub async fn start(pool: PgPool, token: impl Into<String>) -> Result<Self> {
        Self::start_with(pool, token, crate::projection::Narrative::AnswerOnly).await
    }

    /// [`Gateway::start`] with an explicit narrative mode, so a test can also
    /// exercise the progress narrative a real Kohral deployment shows.
    pub async fn start_with(
        pool: PgPool,
        token: impl Into<String>,
        narrative: crate::projection::Narrative,
    ) -> Result<Self> {
        let token = token.into();
        migrate(&pool, MigratePolicy::Apply)
            .await
            .context("running Kevin's migrations")?;

        let data = tempfile::tempdir().context("conformance data dir")?;
        let mut config = config(data.path());
        config.kevin.data_dir = data.path().to_path_buf();
        let config = Arc::new(config);

        let mut registry_config = RegistryConfig::from(&*config);
        registry_config.data_dir = data.path().join("transcripts");
        let mut registry = WorkerRegistry::empty(registry_config, SandboxPolicy::cli_native());
        registry.insert(Arc::new(FakeWorker::new(
            scenario(),
            data.path().join("transcripts"),
        )));

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("binding the conformance gateway")?;
        let address = listener.local_addr().context("gateway address")?;

        let mut gateway = Self {
            store: Arc::new(PgEventStore::new(pool.clone())),
            bus: Arc::new(InProcBus::with_defaults()),
            commands: Arc::new(CommandLog::new(pool.clone())),
            workers: Arc::new(registry),
            workspaces: Arc::new(TempWorkspaces::new(data.path().join("workspaces"))),
            routes: Arc::new(FixedRouter::single(fake_route())),
            task_log: Arc::new(TaskLog::new(pool.clone())),
            ids: Arc::new(UuidV7IdGen),
            auth: Arc::new(TokenVerifier::new(&token)),
            options: KohralOptions {
                narrative,
                work_root: data.path().join("work"),
                upload_root: data.path().join("uploads"),
                ..KohralOptions::from_config(&config, env!("CARGO_PKG_VERSION"))
            },
            config,
            token,
            listener,
            address,
            epoch: None,
            pool,
            _data: data,
        };
        gateway.boot().await?;
        Ok(gateway)
    }

    /// Base URL `contract.py` talks to.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The bearer token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The database the ledger lives in.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Runs the boot sequence: sweep → orchestrator → Kohral listener → serve.
    async fn boot(&mut self) -> Result<()> {
        let restarted = sweep_runtime_restarted(
            &self.pool,
            Arc::clone(&self.store) as Arc<dyn EventStore>,
            Arc::clone(&self.bus) as Arc<dyn kevin_bus::EventBus>,
            Arc::clone(&self.commands) as Arc<dyn kevin_orchestrator::ports::CommandIdempotency>,
            Arc::new(kevin_domain::SystemClock),
            Arc::clone(&self.ids) as Arc<dyn kevin_domain::IdGen>,
            &self.config.kevin.instance_name,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        if !restarted.is_empty() {
            tracing::info!(
                turns = restarted.len(),
                "terminalised turns from a previous boot"
            );
        }

        let roles = Arc::new(RoleRunnerRoles::new(
            Arc::clone(&self.workers),
            Arc::clone(&self.config),
        ));
        let handle = Arc::new(
            Orchestrator::boot(Deps {
                store: Arc::clone(&self.store) as Arc<dyn EventStore>,
                bus: Arc::clone(&self.bus) as Arc<dyn kevin_bus::EventBus>,
                commands: Arc::clone(&self.commands)
                    as Arc<dyn kevin_orchestrator::ports::CommandIdempotency>,
                workers: Arc::clone(&self.workers),
                workspace: Arc::clone(&self.workspaces)
                    as Arc<dyn kevin_orchestrator::ports::WorkspacePort>,
                router: Arc::clone(&self.routes) as Arc<dyn RouterPort>,
                roles: roles as Arc<dyn RolesPort>,
                memory: None,
                evaluator: None,
                config: Arc::clone(&self.config),
                clock: Arc::new(kevin_domain::SystemClock),
                ids: Arc::clone(&self.ids) as Arc<dyn kevin_domain::IdGen>,
                system_context: Vec::new(),
                task_log: Some(Arc::clone(&self.task_log)),
                tick_interval: Duration::from_millis(40),
            })
            .await
            .context("booting the orchestrator")?,
        );

        let kohral = KohralRuntime::start(KohralDeps {
            handle: Arc::clone(&handle),
            pool: self.pool.clone(),
            store: Arc::clone(&self.store) as Arc<dyn EventStore>,
            bus: Arc::clone(&self.bus) as Arc<dyn kevin_bus::EventBus>,
            config: Arc::clone(&self.config),
            auth: Arc::clone(&self.auth),
            workers: Arc::clone(&self.workers),
            options: self.options.clone(),
        })
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

        let router = kohral.router();
        let cancel = CancellationToken::new();
        let socket = self
            .listener
            .try_clone()
            .context("cloning the gateway listener")?;
        socket
            .set_nonblocking(true)
            .context("gateway listener non-blocking")?;
        let socket = tokio::net::TcpListener::from_std(socket).context("tokio listener")?;
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            let served = axum::serve(socket, router)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
            if let Err(error) = served {
                tracing::error!(error = %error, "the Kohral conformance listener stopped");
            }
        });

        self.epoch = Some(Epoch {
            handle,
            kohral,
            server,
            cancel,
        });
        Ok(())
    }

    /// Kills the gateway the way `docker kill` does: nothing is recorded, the
    /// port stops answering, and every in-flight attempt stays non-terminal.
    pub async fn crash(&mut self) {
        if let Some(epoch) = self.epoch.take() {
            epoch.handle.abort();
            epoch.kohral.abort();
            epoch.server.abort();
            let _ = epoch.server.await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Boots a fresh gateway over the same database, on the same port.
    pub async fn restart(&mut self) -> Result<()> {
        if self.epoch.is_some() {
            self.crash().await;
        }
        self.boot().await
    }

    /// Stops the gateway cleanly.
    pub async fn shutdown(mut self) {
        if let Some(epoch) = self.epoch.take() {
            epoch.cancel.cancel();
            epoch.handle.shutdown().await;
            epoch.kohral.shutdown().await;
            let _ = epoch.server.await;
        }
    }

    /// Waits until `/health` answers.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let url = format!("{}/health", self.base_url());
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if let Ok(response) = reqwest_get(&url).await
                && response
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bail!("the conformance gateway did not become ready within {timeout:?}")
    }
}

/// A dependency-free liveness probe (the crate has no HTTP client outside
/// tests): open a connection and read the status line.
async fn reqwest_get(url: &str) -> Result<bool> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let address = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut stream = tokio::net::TcpStream::connect(&address).await?;
    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut buffer = [0u8; 32];
    let read = stream.read(&mut buffer).await?;
    Ok(String::from_utf8_lossy(&buffer[..read]).contains("200"))
}

// ---------------------------------------------------------------------------
// contract.py
// ---------------------------------------------------------------------------

/// Environment variable pointing at a checkout of Kohral's conformance script.
pub const SCRIPT_ENV: &str = "KEVIN_KOHRAL_CONTRACT";

/// Locations tried when [`SCRIPT_ENV`] is unset, relative to `$HOME`.
const DEFAULT_PATHS: [&str; 2] = [
    "workspace/kohral/runtime/conformance/contract.py",
    "kohral/runtime/conformance/contract.py",
];

/// Kohral's `contract.py`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractScript {
    path: PathBuf,
}

impl ContractScript {
    /// Uses an explicit path.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Finds the script: `$KEVIN_KOHRAL_CONTRACT`, then the usual checkouts.
    ///
    /// `None` means "Kohral is not checked out here" — every caller treats
    /// that as *skip*, never as *fail*: Kevin's CI clones Kohral, a laptop may
    /// not have it.
    #[must_use]
    pub fn locate() -> Option<Self> {
        if let Some(path) = std::env::var_os(SCRIPT_ENV) {
            let path = PathBuf::from(path);
            return path.is_file().then_some(Self { path });
        }
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        DEFAULT_PATHS
            .iter()
            .map(|relative| home.join(relative))
            .find(|path| path.is_file())
            .map(|path| Self { path })
    }

    /// Where the script is.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs one phase, returning its captured output.
    pub async fn run(
        &self,
        phase: Phase,
        base_url: &str,
        token: &str,
        run_id_file: Option<&Path>,
        state_timeout: Duration,
    ) -> Result<PhaseReport> {
        let mut command = tokio::process::Command::new("python3");
        command
            .arg(&self.path)
            .arg(phase.as_str())
            .arg("--runtime")
            .arg("hermes")
            .arg("--base-url")
            .arg(base_url)
            .arg("--token")
            .arg(token)
            .arg("--state-timeout")
            .arg(format!("{}", state_timeout.as_secs_f64()));
        if let Some(file) = run_id_file {
            command.arg("--run-id-file").arg(file);
        }
        let output = command
            .output()
            .await
            .with_context(|| format!("running {} {}", self.path.display(), phase.as_str()))?;
        Ok(PhaseReport {
            phase,
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The outcome of one phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseReport {
    /// Which phase.
    pub phase: Phase,
    /// Whether `contract.py` exited zero.
    pub success: bool,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr (the assertion message lives here).
    pub stderr: String,
}

impl PhaseReport {
    /// Turns a failure into an error carrying the assertion text.
    pub fn into_result(self) -> Result<Self> {
        if self.success {
            return Ok(self);
        }
        bail!(
            "contract.py {} failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.phase.as_str(),
            self.stdout.trim(),
            self.stderr.trim()
        )
    }
}

/// Runs `basic`, then `accept-crash`, the crash, and `verify-crash` against
/// `gateway`. This is the whole suite, and it is what
/// `kevin kohral conformance` and `ac_ws22_1` / `ac_ws22_2` both call.
pub async fn run_suite(
    script: &ContractScript,
    gateway: &mut Gateway,
    phases: &[Phase],
) -> Result<BTreeMap<&'static str, PhaseReport>> {
    let mut reports = BTreeMap::new();
    let run_id_file = tempfile::NamedTempFile::new().context("run id file")?;
    let state_timeout = Duration::from_secs(90);
    gateway.wait_ready(Duration::from_secs(30)).await?;

    for phase in phases {
        if *phase == Phase::VerifyCrash {
            // The crash happens *between* the two phases: the runtime dies
            // with the turn accepted and unfinished, and comes back to
            // terminalise it as `runtime_restarted`.
            gateway.crash().await;
            gateway.restart().await?;
            gateway.wait_ready(Duration::from_secs(30)).await?;
        }
        let report = script
            .run(
                *phase,
                &gateway.base_url(),
                gateway.token(),
                Some(run_id_file.path()),
                state_timeout,
            )
            .await?
            .into_result()?;
        reports.insert(phase.as_str(), report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use kevin_worker::fake::{KOHRAL_HOLD_INPUT, KOHRAL_REPLY_INPUT, KOHRAL_REPLY_OUTPUT};

    use super::{ALIAS, Phase, config, scenario};

    #[test]
    fn the_scenario_answers_every_role_and_holds_on_demand() {
        let scenario = scenario();
        // The hold rule must win even though the prompt also contains the
        // goal; it is the first rule for exactly that reason.
        let held = scenario.select(&format!("planner.understanding\n{KOHRAL_HOLD_INPUT}"));
        assert!(held.hold, "the hold hook must beat the role rules");

        for role in ["planner.understanding", "planner.plan", "integrator"] {
            let rule = scenario.select(&format!("{role}\ndo the thing"));
            assert!(
                rule.structured.is_some(),
                "{role} needs a structured answer"
            );
        }

        let task = scenario.select(&format!("Answer\n{KOHRAL_REPLY_INPUT}"));
        assert_eq!(task.reply.as_deref(), Some(KOHRAL_REPLY_OUTPUT));
    }

    #[test]
    fn the_integrator_answer_is_the_output_the_contract_asserts() {
        let scenario = scenario();
        let rule = scenario.select("integrator\nsummarise");
        let value = rule.structured.clone().expect("structured");
        assert_eq!(value["summary"], KOHRAL_REPLY_OUTPUT);
        assert_eq!(value["status"], "integrated");
    }

    #[test]
    fn the_conformance_profile_enables_only_the_fake_worker() {
        let config = config(std::path::Path::new("/tmp/kevin-conformance"));
        assert!(config.workers.fake.enabled);
        assert!(!config.workers.claude.enabled);
        assert!(!config.workers.codex.enabled);
        assert_eq!(config.roles.planner.as_str(), ALIAS);
        assert_eq!(config.kevin.profile, kevin_config::Profile::Kohral);
        assert_eq!(
            config.workspace.integration,
            kevin_config::Integration::None
        );
        assert_eq!(config.models.len(), 1);
    }

    #[test]
    fn the_fixtures_validate_against_the_role_schemas() {
        let scenario = scenario();
        for (role, schema) in [
            (
                "planner.understanding",
                kevin_orchestrator::roles::schemas::understanding(),
            ),
            ("planner.plan", kevin_orchestrator::roles::schemas::plan()),
            (
                "integrator",
                kevin_orchestrator::roles::schemas::integration(),
            ),
        ] {
            let rule = scenario.select(&format!("{role}\nprompt"));
            let value = rule.structured.clone().expect("structured");
            kevin_worker::structured::validate(&value, schema)
                .unwrap_or_else(|error| panic!("{role} fixture violates its schema: {error}"));
        }
    }

    #[test]
    fn the_phases_are_named_the_way_contract_py_spells_them() {
        assert_eq!(Phase::Basic.as_str(), "basic");
        assert_eq!(Phase::AcceptCrash.as_str(), "accept-crash");
        assert_eq!(Phase::VerifyCrash.as_str(), "verify-crash");
    }
}
