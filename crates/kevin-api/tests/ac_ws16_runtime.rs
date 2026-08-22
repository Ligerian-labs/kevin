//! WS-16 acceptance criteria against the **real** orchestrator (WS-08).
//!
//! `ac_ws16_api.rs` pins the HTTP surface down with `kevin_testkit::fake_api`;
//! this suite proves the production wiring: every write goes through
//! [`kevin_api::runtime::OrchestratorRuntime`] into `RunService`/`TaskService`/
//! `QuestionService`, the `Idempotency-Key` becomes the `command_id` that
//! `core.processed_commands` deduplicates on, and drain/readiness reflect the
//! engine's admission gate.
//!
//! Requires Postgres (`DATABASE_URL`); skipped otherwise.

#![cfg(feature = "server")]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use kevin_api::adapters::{ProjectionReads, StoreEvents};
use kevin_api::auth::TokenVerifier;
use kevin_api::port::{EventsPort, ReadPort, RuntimePort};
use kevin_api::runtime::OrchestratorRuntime;
use kevin_api::state::AppState;
use kevin_bus::{EventBus, InProcBus};
use kevin_config::KevinConfig;
use kevin_config::schema::{Integration, ModelEntry, WorkspaceCleanup, WorkspaceStrategy};
use kevin_domain::kinds::Complexity;
use kevin_domain::plan::{Plan, PlanTask};
use kevin_domain::understanding::Understanding;
use kevin_domain::values::RunMode;
use kevin_domain::{ModelAlias, UuidV7IdGen, WorkerKind};
use kevin_orchestrator::projections::{self, ReadModels, TaskLog};
use kevin_orchestrator::testing::{FixedRouter, ScriptedRoles, TempWorkspaces, fake_route};
use kevin_orchestrator::{Deps, Handle, Orchestrator};
use kevin_store::{CommandLog, EventStore, PgEventStore};
use kevin_testkit::fake_worker::{FakeWorker, Scenario};
use kevin_testkit::pg::TestDb;
use kevin_worker::SandboxPolicy;
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const TOKEN: &str = "ws16-runtime-token";

/// Everything a booted API-over-orchestrator test needs, dropped together.
struct Runtime {
    _db: TestDb,
    _tmp: tempfile::TempDir,
    app: Router,
    handle: Arc<Handle>,
    store: Arc<PgEventStore>,
    projections: CancellationToken,
}

impl Runtime {
    async fn boot() -> Self {
        let db = TestDb::new().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = Arc::new(test_config(tmp.path()));

        let store = Arc::new(PgEventStore::new(db.pool().clone()));
        let bus = Arc::new(InProcBus::with_defaults());
        let commands = Arc::new(CommandLog::new(db.pool().clone()));
        let task_log = Arc::new(TaskLog::new(db.pool().clone()));

        let mut registry_cfg = RegistryConfig::from(&*config);
        registry_cfg.data_dir = tmp.path().join("transcripts");
        let mut registry = WorkerRegistry::empty(registry_cfg, SandboxPolicy::cli_native());
        registry.insert(Arc::new(FakeWorker::new(
            Scenario::builtin(),
            tmp.path().join("transcripts"),
        )));

        let roles = Arc::new(
            ScriptedRoles::new()
                .with_understanding(understanding())
                .with_plan(plan()),
        );

        let deps = Deps {
            store: Arc::clone(&store) as Arc<dyn EventStore>,
            bus: Arc::clone(&bus) as Arc<dyn EventBus>,
            commands,
            workers: Arc::new(registry),
            workspace: Arc::new(TempWorkspaces::new(tmp.path().join("workspaces"))),
            router: Arc::new(FixedRouter::single(fake_route())),
            roles,
            memory: None,
            evaluator: None,
            config,
            clock: Arc::new(kevin_domain::SystemClock),
            ids: Arc::new(UuidV7IdGen),
            system_context: Vec::new(),
            task_log: Some(task_log),
            tick_interval: Duration::from_millis(40),
        };
        let handle = Arc::new(Orchestrator::boot(deps).await.expect("boot"));

        // Projections feed every read endpoint.
        let cancel = CancellationToken::new();
        let erased_store: Arc<dyn EventStore> = Arc::clone(&store) as Arc<dyn EventStore>;
        let erased_bus: Arc<dyn EventBus> = Arc::clone(&bus) as Arc<dyn EventBus>;
        projections::spawn_all(db.pool(), &erased_store, &erased_bus, &cancel);

        let read = ReadModels::new(db.pool().clone());
        let state = AppState::builder(
            Arc::new(OrchestratorRuntime::new(Arc::clone(&handle), read.clone()))
                as Arc<dyn RuntimePort>,
            Arc::new(ProjectionReads::new(read)) as Arc<dyn ReadPort>,
            Arc::new(StoreEvents::new(erased_store, erased_bus)) as Arc<dyn EventsPort>,
            Arc::new(TokenVerifier::new(TOKEN)),
        )
        .build();

        Self {
            _db: db,
            _tmp: tmp,
            app: kevin_api::router(state),
            handle,
            store,
            projections: cancel,
        }
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.call_with(method, uri, body, None).await
    }

    async fn call_with(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        idempotency: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json");
        if let Some(key) = idempotency {
            builder = builder.header("idempotency-key", key);
        }
        let request = builder
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .expect("request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("router responds");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// How many `event_type` events the store holds for `run_id`'s stream.
    ///
    /// Asserting on `core.events` rather than on `GET /api/v1/runs` keeps the
    /// check deterministic: the projection is eventually consistent, the store
    /// is not.
    async fn count_events(
        &self,
        aggregate: &'static str,
        id: uuid::Uuid,
        event_type: &str,
    ) -> usize {
        self.store
            .load_stream(&kevin_store::StreamId::new(aggregate, id), 0)
            .await
            .expect("load stream")
            .iter()
            .filter(|event| event.envelope.event_type == event_type)
            .count()
    }

    /// Polls `GET /readyz` until it answers `ready`, or fails.
    ///
    /// Readiness includes a database ping with a one-second budget
    /// (`plan/10` §Health and drain); under a loaded CI the first ping can
    /// legitimately miss that budget, so the assertion is "becomes ready",
    /// not "is ready on the first try".
    async fn wait_until_ready(&self) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut last = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let (status, body) = self.call("GET", "/readyz", None).await;
            if status == StatusCode::OK {
                return body;
            }
            last = body;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("/readyz never became ready (last seen: {last})");
    }

    /// Polls `GET /api/v1/runs/{id}` until `status` matches, or fails.
    async fn wait_for_status(&self, run_id: &str, want: &str) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        let mut last = Value::Null;
        while tokio::time::Instant::now() < deadline {
            let (status, body) = self
                .call("GET", &format!("/api/v1/runs/{run_id}"), None)
                .await;
            if status == StatusCode::OK {
                if body["status"] == want {
                    return body;
                }
                last = body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {run_id} never reached `{want}` (last seen: {last})");
    }

    async fn shutdown(self) {
        self.projections.cancel();
        self.handle.shutdown().await;
    }
}

/// Only the `fake` worker, tiny timings, in-place workspaces.
fn test_config(data_dir: &std::path::Path) -> KevinConfig {
    let mut config = KevinConfig::default();
    config.kevin.data_dir = data_dir.to_path_buf();
    config.kevin.shutdown_grace_period = Duration::from_millis(200);
    config.kevin.auto_approve_plans = false;

    config.models.clear();
    let fake = ModelAlias::new("fake").expect("valid alias");
    config
        .models
        .insert(fake.clone(), ModelEntry::new(WorkerKind::Fake, "fake"));
    config.roles.planner = fake.clone();
    config.roles.clarifier = fake.clone();
    config.roles.judge = fake.clone();
    config.roles.integrator = fake.clone();
    config.roles.default = fake;
    config.roles.effort.clear();

    config.budget.default_run_wall = Duration::from_secs(120);
    config.budget.default_task_wall = Duration::from_secs(15);
    config.budget.max_attempts = 2;

    config.orchestrator.progress_interval = Duration::from_millis(5);
    config.orchestrator.role_call_timeout = Duration::from_secs(10);
    config.orchestrator.evaluation_timeout = Duration::from_secs(10);

    config.workspace.strategy = WorkspaceStrategy::InPlace;
    config.workspace.cleanup = WorkspaceCleanup::Never;
    config.workspace.integration = Integration::Pr;

    config.concurrency.per_worker_kind.clear();
    config
        .concurrency
        .per_worker_kind
        .insert(WorkerKind::Fake, 8);
    config
}

fn understanding() -> Understanding {
    let mut understanding = Understanding::new("add a health endpoint", "the goal is met");
    understanding.complexity = Complexity::Medium;
    understanding
}

fn plan() -> Plan {
    let mut task = PlanTask::new("t1", "implement", "add /healthz");
    "add the endpoint and a test".clone_into(&mut task.instructions);
    Plan::new(vec![task], "one step is enough")
}

// ---------------------------------------------------------------------------
// (1)+(4) the real write path, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_1_commands_reach_the_real_orchestrator_services() {
    kevin_testkit::skip_unless_pg!();
    let runtime = Runtime::boot().await;

    // `POST /runs` really starts a run: the saga picks it up and the scripted
    // planner drives it to `awaiting_plan_approval`.
    let (status, created) = runtime
        .call(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "add a /healthz endpoint" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let run_id = created["id"].as_str().expect("run id").to_owned();
    assert_eq!(created["status"], "received");
    assert_eq!(created["goal"]["text"], "add a /healthz endpoint");
    assert_eq!(created["mode"], "interactive");
    assert!(
        created["budget"]["max_attempts"].as_u64().unwrap_or(0) > 0,
        "the [budget] defaults are applied"
    );

    let awaiting = runtime
        .wait_for_status(&run_id, "awaiting_plan_approval")
        .await;
    assert!(
        awaiting["understanding"].is_object(),
        "the planner's understanding is exposed"
    );
    assert!(awaiting["plan"].is_object(), "the proposed plan is exposed");

    // `POST …/plan/approve` answers from the aggregate, not the projection, so
    // the status it reports is already the post-command one.
    let (status, approved) = runtime
        .call(
            "POST",
            &format!("/api/v1/runs/{run_id}/plan/approve"),
            Some(serde_json::json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{approved}");
    assert_eq!(
        approved["status"], "executing",
        "the read-after-write is the aggregate's state"
    );
    assert!(
        approved["version"].as_u64().unwrap_or(0) > awaiting["version"].as_u64().unwrap_or(0),
        "the aggregate version moved"
    );

    // The saga created the planned task; the board projects it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut tasks = Value::Null;
    while tokio::time::Instant::now() < deadline {
        let (status, body) = runtime
            .call("GET", &format!("/api/v1/runs/{run_id}/tasks"), None)
            .await;
        if status == StatusCode::OK && body.as_array().is_some_and(|t| !t.is_empty()) {
            tasks = body;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let tasks = tasks.as_array().cloned().unwrap_or_default();
    assert_eq!(tasks.len(), 1, "the plan's single task is on the board");
    let task_id = tasks[0]["id"].as_str().expect("task id").to_owned();

    let (status, task) = runtime
        .call("GET", &format!("/api/v1/tasks/{task_id}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["title"], "add /healthz");
    assert_eq!(task["run_id"], run_id);

    // An unknown id is a 404 with the stable code, straight from the service.
    let (status, body) = runtime
        .call(
            "POST",
            &format!("/api/v1/runs/{}/cancel", uuid::Uuid::now_v7()),
            Some(serde_json::json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "run_not_found");

    runtime.shutdown().await;
}

#[tokio::test]
async fn ac_ws16_4_the_idempotency_key_is_the_durable_command_id() {
    kevin_testkit::skip_unless_pg!();
    let runtime = Runtime::boot().await;
    let body = serde_json::json!({ "goal": "idempotent start" });

    let (status, first) = runtime
        .call_with(
            "POST",
            "/api/v1/runs",
            Some(body.clone()),
            Some("cli-ws16-1"),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{first}");

    let (status, replay) = runtime
        .call_with(
            "POST",
            "/api/v1/runs",
            Some(body.clone()),
            Some("cli-ws16-1"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "a replay is 200, not 201");
    assert_eq!(replay["id"], first["id"], "the same run comes back");

    // Exactly one `run.started` reached the store: `core.processed_commands`
    // deduplicated on the command id derived from the key. The assertion goes
    // to `core.events` rather than to `GET /api/v1/runs`, because the
    // projection is eventually consistent and the store is not.
    let run_uuid: uuid::Uuid = first["id"].as_str().expect("run id").parse().expect("uuid");
    assert_eq!(
        runtime.count_events("run", run_uuid, "run.started").await,
        1,
        "the replay appended nothing"
    );

    // And the key really is the durable command id (uuid v5 of the key), so a
    // replay from another process hits the same command-log row.
    assert_eq!(
        run_uuid,
        kevin_api::state::Idempotency::command_id("cli-ws16-1").as_uuid(),
        "the run id is derived from the command id"
    );

    // A different body under the same key is a conflict.
    let (status, conflict) = runtime
        .call_with(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "different" })),
            Some("cli-ws16-1"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["code"], "idempotency_conflict");

    runtime.shutdown().await;
}

// ---------------------------------------------------------------------------
// drain and readiness against the engine's admission gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_1_drain_and_readiness_follow_the_admission_gate() {
    kevin_testkit::skip_unless_pg!();
    let runtime = Runtime::boot().await;

    let ready = runtime.wait_until_ready().await;
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["db"], true, "the database ping succeeded");
    assert_eq!(ready["workers_ok"], true, "the fake worker is registered");

    let (status, drain) = runtime
        .call("POST", "/api/v1/maintenance/drain", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(drain["draining"], true);
    assert!(
        !runtime.handle.is_admitting(),
        "the engine really stopped admitting"
    );

    // `/healthz` stays 200 while `/readyz` turns 503 (plan/10 §Health and drain).
    let (status, _) = runtime.call("GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ready) = runtime.call("GET", "/readyz", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(ready["draining"], true);

    let (status, body) = runtime
        .call(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "while draining" })),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "draining");

    let (status, drain) = runtime
        .call("DELETE", "/api/v1/maintenance/drain", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(drain["draining"], false);
    assert!(runtime.handle.is_admitting(), "admission reopened");

    let (status, _) = runtime
        .call(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "after undrain" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    runtime.shutdown().await;
}

// ---------------------------------------------------------------------------
// (2) SSE over the real store and bus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_2_sse_streams_the_real_run_events() {
    kevin_testkit::skip_unless_pg!();
    let runtime = Runtime::boot().await;

    let (status, created) = runtime
        .call(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "stream me" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let run_id = created["id"].as_str().expect("run id").to_owned();
    runtime
        .wait_for_status(&run_id, "awaiting_plan_approval")
        .await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/runs/{run_id}/events?types=run.*"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("last-event-id", "0")
        .body(Body::empty())
        .expect("request");
    let response = runtime
        .app
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    let mut decoder = kevin_api::sse_wire::Decoder::new();
    let mut messages = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while messages.len() < 2 {
        use futures::StreamExt;
        let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        messages.extend(decoder.push(&chunk));
    }
    assert!(messages.len() >= 2, "the catch-up replayed real events");
    assert_eq!(messages[0].name(), "run.started");

    let mut last = 0;
    for message in &messages {
        let event: kevin_api::dto::EventDto =
            serde_json::from_str(&message.data).expect("EventDto");
        assert!(event.position > last, "positions strictly increase");
        assert_eq!(event.correlation_id.to_string(), run_id);
        last = event.position;
    }

    drop(stream);
    runtime.shutdown().await;
}

// ---------------------------------------------------------------------------
// headless runs approve their own plan; the API reports the real mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_1_headless_runs_are_started_in_headless_mode() {
    kevin_testkit::skip_unless_pg!();
    let runtime = Runtime::boot().await;

    let (status, created) = runtime
        .call(
            "POST",
            "/api/v1/runs",
            Some(serde_json::json!({ "goal": "no humans", "mode": "headless" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["mode"], "headless");

    let run_id = created["id"].as_str().expect("run id").to_owned();
    let run = runtime
        .handle
        .run_service()
        .load(run_id.parse().expect("uuid"))
        .await
        .expect("load");
    assert_eq!(run.mode(), Some(&RunMode::Headless));

    runtime.shutdown().await;
}
