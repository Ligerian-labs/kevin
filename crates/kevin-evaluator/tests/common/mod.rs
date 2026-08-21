//! Shared fixtures for the WS-19 acceptance tests.
//!
//! The judge always runs on the **fake worker** (`plan/11-testing.md`): a real
//! CLI is never invoked by the suite.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kevin_config::{Evaluation as EvaluationCfg, ModelEntry, Role, Roles};
use kevin_domain::{Effort, ModelAlias, Route, RunId, TaskId, TaskKind, WorkerKind};
use kevin_evaluator::{
    AutoApply, Evaluator, EvaluatorConfig, Evidence, InMemoryEvaluationRepo, InMemoryLessons,
    InMemoryRouter,
};
use kevin_testkit::fake_worker::{FakeWorker, Rule, Scenario};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::{SandboxPolicy, Workspace};
use tempfile::TempDir;

/// The alias the work under judgement ran on.
pub const EXECUTOR_ALIAS: &str = "fake";
/// A second judge-capable alias on a different provider model.
pub const OTHER_JUDGE_ALIAS: &str = "fake-judge";

/// `judge-accept.json` — a good result, a generous `overall`, two lessons and
/// two proposals (one routing, one prompt).
pub const GOLDEN_ACCEPT: &str = include_str!("../fixtures/judge/judge-accept.json");
/// `judge-generous.json` — poor scores but an `accept` verdict; the recomputed
/// score must win.
pub const GOLDEN_GENEROUS: &str = include_str!("../fixtures/judge/judge-generous.json");
/// `judge-run.json` — a run-level answer on the `default` rubric.
pub const GOLDEN_RUN: &str = include_str!("../fixtures/judge/judge-run.json");

/// An alias, panicking on an invalid one.
pub fn alias(name: &str) -> ModelAlias {
    ModelAlias::new(name).expect("valid alias")
}

/// `[models]` with one executor alias and one judge-tagged alias, both on the
/// fake worker but on different provider models.
pub fn models(with_second_judge: bool) -> BTreeMap<ModelAlias, ModelEntry> {
    let mut models = BTreeMap::new();
    models.insert(
        alias(EXECUTOR_ALIAS),
        ModelEntry::new(WorkerKind::Fake, "fake-executor"),
    );
    if with_second_judge {
        let mut judge = ModelEntry::new(WorkerKind::Fake, "fake-judge-model");
        judge.tags = vec!["judge".to_owned()];
        models.insert(alias(OTHER_JUDGE_ALIAS), judge);
    }
    models
}

/// `[roles]` with `judge = fake`.
pub fn roles() -> Roles {
    Roles {
        planner: alias(EXECUTOR_ALIAS),
        clarifier: alias(EXECUTOR_ALIAS),
        judge: alias(EXECUTOR_ALIAS),
        integrator: alias(EXECUTOR_ALIAS),
        default: alias(EXECUTOR_ALIAS),
        effort: BTreeMap::from([(Role::Judge, Effort::High)]),
    }
}

/// The evaluator configuration used by the acceptance tests.
pub fn config(with_second_judge: bool) -> EvaluatorConfig {
    EvaluatorConfig {
        evaluation: EvaluationCfg::default(),
        roles: roles(),
        models: models(with_second_judge),
        timeout: Duration::from_secs(10),
    }
}

/// A registry holding only the fake worker, replying `reply` to everything.
pub fn registry(reply: &str, with_second_judge: bool) -> (TempDir, WorkerRegistry) {
    let dir = tempfile::tempdir().expect("tempdir");
    let scenario = Scenario::replying(reply).with_default(Rule::replying(reply));
    let worker = Arc::new(FakeWorker::new(scenario, dir.path()));
    let mut cfg = RegistryConfig::fake_only(dir.path());
    cfg.models = models(with_second_judge);
    let mut registry = WorkerRegistry::empty(cfg, SandboxPolicy::cli_native());
    registry.insert(worker);
    (dir, registry)
}

/// Everything one acceptance test needs, wired around the in-memory repo.
pub struct Fixture {
    /// Keeps the fake worker's transcript directory alive.
    pub dir: TempDir,
    /// The service under test.
    pub evaluator: Evaluator,
    /// The recorded evaluations and proposals.
    pub repo: Arc<InMemoryEvaluationRepo>,
    /// What auto-apply sent to routing.
    pub router: Arc<InMemoryRouter>,
    /// What auto-apply sent to memory.
    pub memory: Arc<InMemoryLessons>,
}

impl Fixture {
    /// A fixture whose judge always answers `reply`.
    pub fn new(reply: &str) -> Self {
        Self::with(reply, config(true), AutoApplyParts::Both)
    }

    /// A fixture with an explicit configuration and auto-apply policy.
    pub fn with(reply: &str, cfg: EvaluatorConfig, parts: AutoApplyParts) -> Self {
        let (dir, registry) = registry(reply, cfg.models.len() > 1);
        let repo = Arc::new(InMemoryEvaluationRepo::new());
        let router = Arc::new(InMemoryRouter::new());
        let memory = Arc::new(InMemoryLessons::new());
        let auto = AutoApply::new(parts.parts())
            .with_router(router.clone())
            .with_memory(memory.clone());
        let evaluator = Evaluator::new(
            cfg,
            Arc::new(registry),
            Workspace::in_place(dir.path()),
            repo.clone(),
            auto,
        );
        Self {
            dir,
            evaluator,
            repo,
            router,
            memory,
        }
    }
}

/// Which auto-apply parts a fixture allows.
#[derive(Debug, Clone, Copy)]
pub enum AutoApplyParts {
    /// `["routing", "memory"]` — the default.
    Both,
    /// `[]` — nothing may change without a human.
    None,
}

impl AutoApplyParts {
    fn parts(self) -> Vec<kevin_config::AutoApply> {
        match self {
            AutoApplyParts::Both => vec![
                kevin_config::AutoApply::Routing,
                kevin_config::AutoApply::Memory,
            ],
            AutoApplyParts::None => Vec::new(),
        }
    }
}

/// The route the judged work ran on.
pub fn executor_route() -> Route {
    Route::new(WorkerKind::Fake, alias(EXECUTOR_ALIAS))
}

/// Evidence for an `implement` task (rubric `code`).
pub fn implement_evidence() -> Evidence {
    Evidence::new("Add a /healthz endpoint to the axum app and a test for it")
        .with_acceptance_criteria(["GET /healthz returns 200", "an integration test covers it"])
        .with_diff(
            "diff --git a/src/lib.rs b/src/lib.rs\n+async fn healthz() -> &'static str { \"ok\" }",
        )
        .with_test_output("running 1 test\ntest healthz ... ok\n\ntest result: ok. 1 passed")
        .with_transcript_summary("Added the handler, registered the route, added one test.")
}

/// A task subject with its ids.
pub fn task_ids() -> (RunId, TaskId) {
    (RunId::new(), TaskId::new())
}

/// The `implement` task kind.
pub fn implement() -> TaskKind {
    TaskKind::Implement
}
