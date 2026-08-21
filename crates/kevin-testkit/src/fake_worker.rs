//! Fake worker helpers (WS-05): programmatic scenarios (`plan/04-workers.md`
//! §Adapter: `fake`), canned scenarios, registries pre-wired with the fake
//! worker, request builders and event assertions.
//!
//! ```no_run
//! use kevin_testkit::fake_worker::{self, scenarios};
//!
//! # async fn demo() {
//! let fx = fake_worker::FakeWorkerFixture::new(scenarios::happy_path());
//! let req = fake_worker::request("implement the login form").build();
//! let (events, outcome) = fake_worker::run(&*fx.worker, req).await;
//! assert!(outcome.is_success());
//! assert!(kevin_worker::worker::check_contract(&events).is_ok());
//! # }
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kevin_domain::{AttemptId, FailureClass, ModelAlias, RunId, TaskId, TaskKind, WorkerKind};
pub use kevin_worker::fake::{
    FailSpec, FakeWorker, KOHRAL_HOLD_INPUT, KOHRAL_REPLY_INPUT, KOHRAL_REPLY_OUTPUT, Matcher,
    Rule, Scenario, ScriptedEvent,
};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
pub use kevin_worker::worker::{ContractViolation, check_contract};
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, ModelEntry, Route, SandboxPolicy,
    TaskAttemptRequest, TaskSpec, Usage, Worker, WorkerEvent, WorkerOutcome, Workspace,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// Canned scenarios.
pub mod scenarios {
    use super::{FailureClass, Rule, Scenario, Usage};

    /// Default `done`; `implement …` replies with a tool call + text; `plan`
    /// replies with a small structured plan.
    #[must_use]
    pub fn happy_path() -> Scenario {
        Scenario::replying("done")
            .with_default(Rule::replying("done").usage(Usage::tokens(10, 5)))
            .rule(
                Rule::matching("/implement/")
                    .tool_call("edit", "src/lib.rs")
                    .text("Implemented.")
                    .reply("Implemented as requested.")
                    .usage(Usage::tokens(120, 40)),
            )
            .rule(
                Rule::matching("plan")
                    .structured(serde_json::json!({"tasks": [{"title": "do it"}]}))
                    .reply("{\"tasks\": [{\"title\": \"do it\"}]}"),
            )
    }

    /// Every prompt fails with `Transient` (`simulated 429`); `succeed`
    /// succeeds — handy for retry tests.
    #[must_use]
    pub fn fail_transient() -> Scenario {
        Scenario::replying("done")
            .with_default(Rule::default().fail(FailureClass::Transient, "simulated 429"))
            .rule(Rule::matching("succeed").reply("done"))
    }

    /// Every prompt fails with `Permanent`.
    #[must_use]
    pub fn fail_permanent() -> Scenario {
        Scenario::replying("done")
            .with_default(Rule::default().fail(FailureClass::Permanent, "simulated invalid spec"))
    }

    /// Every prompt holds (`Started`, then nothing until cancelled / timed out).
    #[must_use]
    pub fn hold() -> Scenario {
        Scenario::replying("done").with_default(Rule::default().hold())
    }

    /// `fenced` replies with fenced JSON needing repair, `violate` with JSON
    /// that breaks [`OUTPUT_SCHEMA`](super::OUTPUT_SCHEMA), `native` returns
    /// structured output directly.
    #[must_use]
    pub fn structured_output() -> Scenario {
        Scenario::replying("{\"status\": \"ok\"}")
            .rule(Rule::matching("fenced").reply(
                "Result:\n```json\n{\"status\": \"ok\", \"files\": [\"a.rs\",],}\n```\nThanks.",
            ))
            .rule(Rule::matching("violate").reply("{\"status\": \"maybe\"}"))
            .rule(
                Rule::matching("native")
                    .reply("done")
                    .structured(serde_json::json!({"status": "ok", "files": []})),
            )
    }

    /// The built-in scenario with the Kohral conformance hooks.
    #[must_use]
    pub fn kohral_conformance() -> Scenario {
        Scenario::builtin()
    }
}

/// The JSON schema the `structured_output` scenario targets.
#[must_use]
pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string", "enum": ["ok", "error"] },
            "files": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
    })
}

/// Schema constant name used in docs.
pub const OUTPUT_SCHEMA: &str = "kevin_testkit::fake_worker::output_schema()";

/// A fake worker with its own temp `data_dir`, plus a registry containing only it.
#[derive(Debug)]
pub struct FakeWorkerFixture {
    /// Temp dir holding transcripts (dropped with the fixture).
    pub dir: TempDir,
    /// The fake worker.
    pub worker: Arc<FakeWorker>,
    /// Registry with only the fake worker (`fake` alias), cli-native policy.
    pub registry: WorkerRegistry,
}

impl FakeWorkerFixture {
    /// Builds the fixture for `scenario`.
    #[must_use]
    pub fn new(scenario: Scenario) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let worker = Arc::new(FakeWorker::new(scenario, dir.path()));
        let cfg = RegistryConfig::fake_only(dir.path());
        let mut registry = WorkerRegistry::empty(cfg, SandboxPolicy::cli_native());
        registry.insert(worker.clone());
        Self {
            dir,
            worker,
            registry,
        }
    }

    /// Built-in scenario.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(Scenario::builtin())
    }

    /// The transcript root.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }
}

/// Builds a [`WorkerRegistry`] with only the fake worker for `scenario`
/// (transcripts under the returned temp dir — keep it alive).
#[must_use]
pub fn fake_registry(scenario: Scenario) -> (TempDir, WorkerRegistry) {
    let fx = FakeWorkerFixture::new(scenario);
    (fx.dir, fx.registry)
}

/// Builder for a [`TaskAttemptRequest`] routed to the fake worker.
#[derive(Debug, Clone)]
pub struct RequestBuilder {
    req: TaskAttemptRequest,
}

impl RequestBuilder {
    /// Sets the title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.req.spec.title = title.into();
        self
    }

    /// Sets the task kind.
    #[must_use]
    pub fn kind(mut self, kind: TaskKind) -> Self {
        self.req.kind = kind;
        self
    }

    /// Sets the output schema.
    #[must_use]
    pub fn output_schema(mut self, schema: serde_json::Value) -> Self {
        self.req.spec.output_schema = Some(schema);
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.req.budget.timeout = timeout;
        self
    }

    /// Sets the workspace root.
    #[must_use]
    pub fn workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.req.workspace = Workspace::in_place(root);
        self
    }

    /// Sets the cancellation token.
    #[must_use]
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.req.cancel = cancel;
        self
    }

    /// Sets the ids (for deterministic transcript paths).
    #[must_use]
    pub fn ids(mut self, run_id: RunId, task_id: TaskId, attempt_id: AttemptId) -> Self {
        self.req.run_id = run_id;
        self.req.task_id = task_id;
        self.req.attempt_id = attempt_id;
        self
    }

    /// Sets the context.
    #[must_use]
    pub fn context(mut self, context: AttemptContext) -> Self {
        self.req.context = context;
        self
    }

    /// The request.
    #[must_use]
    pub fn build(self) -> TaskAttemptRequest {
        self.req
    }
}

/// A request with `instructions = prompt`, fresh ids, the `fake` route/model,
/// the temp dir as workspace and a 30 s timeout.
#[must_use]
pub fn request(prompt: &str) -> RequestBuilder {
    RequestBuilder {
        req: TaskAttemptRequest {
            attempt_id: AttemptId::new(),
            task_id: TaskId::new(),
            run_id: RunId::new(),
            kind: TaskKind::Implement,
            spec: TaskSpec::new("", prompt),
            route: Route {
                worker: WorkerKind::Fake,
                model: ModelAlias::new("fake").expect("valid alias"),
                effort: None,
            },
            model: ModelEntry::new(WorkerKind::Fake, "fake"),
            workspace: Workspace::in_place(std::env::temp_dir()),
            context: AttemptContext::default(),
            env: EnvAllowlist::new(["PATH", "HOME"]),
            budget: AttemptBudget::with_timeout(Duration::from_secs(30)),
            cancel: CancellationToken::new(),
        },
    }
}

/// Starts `req` on `worker`, collects every event and the outcome.
///
/// # Panics
/// When the worker cannot be started.
pub async fn run(
    worker: &dyn Worker,
    req: TaskAttemptRequest,
) -> (Vec<WorkerEvent>, WorkerOutcome) {
    worker
        .start(req)
        .await
        .expect("fake worker starts")
        .collect()
        .await
}

/// The `snake_case` names of `events` (`started`, `tool_call`, …).
#[must_use]
pub fn event_kinds(events: &[WorkerEvent]) -> Vec<&'static str> {
    events.iter().map(WorkerEvent::kind_name).collect()
}

/// Asserts the stream contract and that the outcome mirrors the terminal event.
///
/// # Panics
/// On any violation.
pub fn assert_contract(events: &[WorkerEvent], outcome: &WorkerOutcome) {
    check_contract(events).expect("worker stream contract");
    let terminal = events.last().expect("terminal event");
    match (terminal, outcome) {
        (WorkerEvent::Final { text, .. }, WorkerOutcome::Succeeded { text: out, .. }) => {
            assert_eq!(text, out, "Final.text must equal Succeeded.text");
        }
        (
            WorkerEvent::Failed { class, .. },
            WorkerOutcome::Failed {
                class: out_class, ..
            },
        ) => {
            assert_eq!(class, out_class, "Failed.class must equal outcome class");
        }
        (ev, out) => panic!("terminal event {ev:?} does not match outcome {out:?}"),
    }
}
