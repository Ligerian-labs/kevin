//! WS-16 acceptance criterion (1), Postgres half: the read endpoints served by
//! the real read models.
//!
//! `ac_ws16_api.rs` proves the HTTP surface against `kevin_testkit::fake_api`;
//! this suite proves the other half — that
//! [`kevin_api::adapters::ProjectionReads`] turns real `orch.*` rows into the
//! DTOs of `plan/07-api-and-tui.md`, and that the SSE catch-up reads real
//! `core.events` through [`kevin_api::adapters::StoreEvents`].
//!
//! Requires Postgres (`DATABASE_URL`); skipped otherwise.

#![cfg(feature = "server")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use kevin_api::adapters::{ProjectionReads, StoreEvents};
use kevin_api::auth::TokenVerifier;
use kevin_api::dto::{Page, RunDto, RunSummaryDto, TaskDto};
use kevin_api::port::{EventsPort, ReadPort, RuntimePort};
use kevin_api::state::AppState;
use kevin_bus::{EventBus, InProcBus};
use kevin_domain::aggregate::EventMeta;
use kevin_domain::ids::EventId;
use kevin_domain::{Actor, DomainEvent};
use kevin_orchestrator::projections::{self, ProjectionRunner, ReadModels};
use kevin_store::{EventStore, NewEvent, PgEventStore, StreamId};
use kevin_testkit::fake_api::{self, FakeRuntime};
use kevin_testkit::given_when_then::{ids, question, run, task};
use kevin_testkit::pg::TestDb;
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

/// One event to append, with the stream it belongs to.
struct Fixture {
    aggregate_type: &'static str,
    aggregate_id: Uuid,
    event: DomainEvent,
}

fn on(aggregate_type: &'static str, aggregate_id: Uuid, event: impl Into<DomainEvent>) -> Fixture {
    Fixture {
        aggregate_type,
        aggregate_id,
        event: event.into(),
    }
}

/// A complete run: understanding → one question → plan → one task with a
/// retry → integration → evaluation → completion.
fn scenario() -> Vec<Fixture> {
    let run_id = ids::run_id().as_uuid();
    let question_id = ids::question_id(1).as_uuid();
    let task_1 = ids::task_id(1).as_uuid();
    let usage = kevin_testkit::given_when_then::values::usage();

    vec![
        on("run", run_id, run::started()),
        on("run", run_id, run::understanding_started()),
        on(
            "run",
            run_id,
            run::understanding_completed_with_questions(vec![ids::question_id(1)]),
        ),
        on("question", question_id, question::asked()),
        on("question", question_id, question::answered()),
        on(
            "run",
            run_id,
            run::question_answered(ids::question_id(1), 0),
        ),
        on("run", run_id, run::plan_proposed()),
        on("run", run_id, run::plan_approved()),
        on("run", run_id, run::execution_started()),
        on("task", task_1, task::created()),
        on("task", task_1, task::routed()),
        on("task", task_1, task::attempt_started(1)),
        on("task", task_1, task::progressed(1)),
        on(
            "task",
            task_1,
            task::attempt_failed(1, kevin_domain::kinds::FailureClass::Transient, true),
        ),
        on("task", task_1, task::retried(2)),
        on("task", task_1, task::attempt_started(2)),
        on("task", task_1, task::attempt_succeeded(2)),
        on(
            "run",
            run_id,
            run::task_terminal_noted(ids::task_id(1), true, usage),
        ),
        on("run", run_id, run::integrated()),
        on("run", run_id, run::evaluated()),
        on("run", run_id, run::completed(usage)),
    ]
}

async fn append_scenario(store: &PgEventStore) {
    let run_id = ids::run_id().as_uuid();
    for fixture in scenario() {
        let stream = StreamId::new(fixture.aggregate_type, fixture.aggregate_id);
        let version = store.stream_version(&stream).await.expect("stream version");
        let event = NewEvent {
            event_id: EventId::new(),
            event_type: fixture.event.event_type(),
            schema_version: fixture.event.schema_version(),
            occurred_at: kevin_testkit::given_when_then::fixture_time()
                + chrono::TimeDelta::seconds(i64::try_from(version).unwrap_or(0)),
            correlation_id: run_id,
            causation_id: None,
            actor: Actor::system("test"),
            payload: serde_json::to_value(&fixture.event).expect("payload"),
        };
        store
            .append(&stream, version, std::slice::from_ref(&event))
            .await
            .expect("append");
    }
}

async fn project(db: &TestDb, store: &PgEventStore) {
    let erased: Arc<dyn EventStore> = Arc::new(store.clone());
    for projection in projections::all() {
        let mut runner = ProjectionRunner::new(projection, db.pool().clone(), Arc::clone(&erased));
        runner.load_checkpoint().await.expect("checkpoint");
        runner.catch_up().await.expect("catch up");
    }
}

/// The API in front of the real read models; writes still go to the fake
/// runtime (WS-08 owns the write side).
fn app(db: &TestDb, store: &PgEventStore, bus: Arc<InProcBus>) -> axum::Router {
    let runtime = Arc::new(FakeRuntime::new());
    let reads = Arc::new(ProjectionReads::new(ReadModels::new(db.pool().clone())));
    let events = Arc::new(StoreEvents::new(
        Arc::new(store.clone()) as Arc<dyn EventStore>,
        bus as Arc<dyn EventBus>,
    ));
    let state = AppState::builder(
        runtime as Arc<dyn RuntimePort>,
        reads as Arc<dyn ReadPort>,
        events as Arc<dyn EventsPort>,
        Arc::new(TokenVerifier::new(fake_api::TOKEN)),
    )
    .build();
    kevin_api::router(state)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .body(Body::empty())
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("router responds");
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

#[tokio::test]
async fn ac_ws16_1_read_endpoints_serve_the_real_read_models() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store).await;
    project(&db, &store).await;

    let app = app(&db, &store, Arc::new(InProcBus::with_defaults()));
    let run_id = ids::run_id();
    let task_id = ids::task_id(1);
    let question_id = ids::question_id(1);

    // GET /runs
    let (status, body) = get(&app, "/api/v1/runs").await;
    assert_eq!(status, StatusCode::OK);
    let page: Page<RunSummaryDto> = serde_json::from_value(body).expect("Page<RunSummaryDto>");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, run_id);
    // The plan declares two tasks; only one was actually created in this
    // fixture, and it succeeded.
    assert_eq!(page.items[0].task_counts.total, 2);
    assert_eq!(page.items[0].task_counts.succeeded, 1);

    // GET /runs/{id} — the projected understanding, plan and tasks.
    let (status, body) = get(&app, &format!("/api/v1/runs/{run_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let run: RunDto = serde_json::from_value(body).expect("RunDto");
    assert_eq!(run.status, kevin_api::dto::RunStatusDto::Completed);
    assert!(run.understanding.is_some(), "understanding is projected");
    assert!(run.plan.is_some(), "plan is projected");
    assert_eq!(run.tasks.len(), 1);
    assert!(run.open_questions.is_empty(), "the question was answered");
    assert!(run.evaluation.is_some(), "the judge verdict is summarised");
    assert!(
        run.budget.max_attempts > 0,
        "the budget JSON decodes into BudgetDto"
    );

    // GET /runs/{id}/tasks and /tasks/{id} — attempts, route and criteria.
    let (status, body) = get(&app, &format!("/api/v1/runs/{run_id}/tasks")).await;
    assert_eq!(status, StatusCode::OK);
    let tasks: Vec<TaskDto> = serde_json::from_value(body).expect("Vec<TaskDto>");
    assert_eq!(tasks.len(), 1);

    let (status, body) = get(&app, &format!("/api/v1/tasks/{task_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let task: TaskDto = serde_json::from_value(body).expect("TaskDto");
    assert_eq!(task.status, "succeeded");
    assert_eq!(task.attempts.len(), 2, "the retry is visible");
    assert_eq!(task.attempts[0].status, "failed");
    assert_eq!(
        task.attempts[0]
            .failure
            .as_ref()
            .map(|failure| failure.class.as_str()),
        Some("transient")
    );
    assert_eq!(task.attempts[1].status, "succeeded");
    assert!(task.route.is_some(), "the route JSON decodes into RouteDto");
    assert!(!task.acceptance_criteria.is_empty());

    // GET /questions
    let (status, body) = get(&app, "/api/v1/questions?status=answered").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));

    let (status, body) = get(&app, &format!("/api/v1/questions/{question_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "answered");
    assert!(body["answer"].is_object(), "the answer is projected");

    // GET /cost — the ledger, grouped.
    let (status, body) = get(&app, "/api/v1/cost?group_by=model").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["total_tokens"].as_u64().unwrap_or(0) > 0,
        "the cost ledger has rows"
    );
    assert!(
        body["rows"].as_array().is_some_and(|rows| !rows.is_empty()),
        "grouped rows"
    );

    // Money is a decimal string, never a float (plan/07 §Conventions).
    if let Some(usd) = body["rows"][0].get("usd")
        && !usd.is_null()
    {
        assert!(usd.is_string(), "money is a decimal string, got {usd}");
    }

    // Unknown ids are 404 with the stable code.
    let (status, body) = get(&app, &format!("/api/v1/tasks/{}", Uuid::now_v7())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "task_not_found");
}

#[tokio::test]
async fn ac_ws16_2_sse_catch_up_reads_the_real_event_store() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store).await;
    project(&db, &store).await;

    let app = app(&db, &store, Arc::new(InProcBus::with_defaults()));
    let run_id = ids::run_id();

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/runs/{run_id}/events?types=run.*"))
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("last-event-id", "0")
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    // The scenario has 12 `run.*` events; read the first few and check that
    // positions are strictly increasing and the payloads decode as `EventDto`.
    let mut decoder = kevin_api::sse_wire::Decoder::new();
    let mut messages = Vec::new();
    let mut stream = response.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while messages.len() < 3 {
        use futures::StreamExt;
        let Ok(Some(Ok(chunk))) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        messages.extend(decoder.push(&chunk));
    }
    assert!(messages.len() >= 3, "the catch-up replayed the history");
    assert_eq!(messages[0].name(), "run.started");

    let mut last = 0;
    for message in &messages {
        let event: kevin_api::dto::EventDto =
            serde_json::from_str(&message.data).expect("EventDto");
        assert!(event.position > last, "positions are strictly increasing");
        assert_eq!(event.correlation_id, run_id.as_uuid());
        assert!(event.event_type.starts_with("run."), "the ?types filter");
        assert_eq!(
            message.id.as_deref(),
            Some(event.position.to_string().as_str()),
            "the SSE id is the global position"
        );
        last = event.position;
    }
}
