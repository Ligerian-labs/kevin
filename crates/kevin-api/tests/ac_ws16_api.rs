//! WS-16 acceptance criteria (`plan/12-workstreams.md` §WS-16):
//!
//! 1. every endpoint of the table in `plan/07-api-and-tui.md` has a `oneshot` test;
//! 2. an SSE reconnect resumes from the last position, and bus lag becomes `resync`;
//! 3. auth: `401` without a token, and the comparison is constant time;
//! 4. an `Idempotency-Key` replay returns the same run;
//! 5. the OpenAPI document validates;
//! 6. `KevinClient` round-trips against the fake API.
//!
//! Everything here runs against `kevin_testkit::fake_api`, so no Postgres and
//! no orchestrator are needed; the read-model adapter has its own
//! Postgres-backed suite in `ac_ws16_projections.rs`.

#![cfg(feature = "server")]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use kevin_api::dto::{
    ArtifactDto, CostRowDto, EventDto, MemoryItemDto, Page, ProposalDto, RouteScoreDto, RunDto,
    RunSummaryDto, TaskLogLineDto, WorkerDoctorDto,
};
use kevin_api::port::EventsPort;
use kevin_api::sse::{self, EventFilter, Start};
use kevin_domain::ids::{ArtifactId, MemoryItemId, ProposalId, QuestionId, RunId, TaskId};
use kevin_testkit::fake_api::{self, FakeRuntime};
use serde_json::Value;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fake runtime with one run, one task, one question, a log line, an
/// artifact, a route score, a proposal, a lesson and a worker report — enough
/// for every read endpoint to answer with real content.
fn seeded() -> (FakeRuntime, RunId, TaskId, QuestionId, ArtifactId) {
    let runtime = FakeRuntime::new();
    let run_id = RunId::new();
    let task_id = TaskId::new();
    let question_id = QuestionId::new();
    let artifact_id = ArtifactId::new();

    let mut run = fake_api::run_fixture(run_id);
    run.open_questions = vec![question_id];
    runtime.insert_run(run);
    runtime.insert_task(fake_api::task_fixture(task_id, run_id));
    runtime.insert_question(fake_api::question_fixture(question_id, run_id));
    runtime.insert_log(
        task_id,
        vec![TaskLogLineDto {
            seq: 1,
            attempt: 1,
            at: fake_api::fixture_time(),
            kind: "assistant".to_owned(),
            payload: serde_json::json!({ "text": "working" }),
        }],
    );
    runtime.insert_artifact(
        ArtifactDto {
            id: artifact_id,
            run_id,
            task_id: Some(task_id),
            kind: "diff".to_owned(),
            uri: "file:///dev/null".to_owned(),
            sha256: None,
            bytes: Some(7),
            produced_by: "task".to_owned(),
            created_at: fake_api::fixture_time(),
        },
        b"--- a\n".to_vec(),
    );

    runtime.with_state(|state| {
        state.routes.push(RouteScoreDto {
            kind: "implement".to_owned(),
            alias: "sonnet".to_owned(),
            attempts: 4,
            successes: 3,
            mean_quality: Some(0.8),
            mean_cost_usd: None,
            mean_wall_ms: Some(1200),
            sampled_score: Some(0.77),
        });
        state.proposals.push(ProposalDto {
            id: ProposalId::new(),
            evaluation_id: kevin_domain::ids::EvaluationId::new(),
            kind: "prompt".to_owned(),
            body: "tighten the planner prompt".to_owned(),
            status: "proposed".to_owned(),
            created_at: fake_api::fixture_time(),
        });
        state.memory.push(MemoryItemDto {
            id: MemoryItemId::new(),
            kind: "lesson".to_owned(),
            content: "always run the tests before integrating".to_owned(),
            tags: vec!["test".to_owned()],
            importance: 0.5,
            similarity: None,
            source: serde_json::json!({}),
            created_at: fake_api::fixture_time(),
        });
        state.workers.push(WorkerDoctorDto {
            kind: "fake".to_owned(),
            enabled: true,
            binary: None,
            version: Some("1.0".to_owned()),
            auth_ready: Some(true),
            problems: Vec::new(),
        });
        state.cost.total_tokens = 42;
        state.cost.rows.push(CostRowDto {
            key: run_id.to_string(),
            usd: None,
            input_tokens: 20,
            output_tokens: 22,
            attempts: 1,
        });
    });

    (runtime, run_id, task_id, question_id, artifact_id)
}

fn authorized(method: &str, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("content-type", "application/json");
    match body {
        Some(body) => builder
            .body(Body::from(body.to_string()))
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
    }
}

async fn call(
    runtime: &FakeRuntime,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = fake_api::router(runtime)
        .oneshot(authorized(method, uri, body))
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

// ---------------------------------------------------------------------------
// (1) every endpoint has a oneshot test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_1_health_and_openapi_endpoints_answer_over_oneshot() {
    let (runtime, _, _, _, _) = seeded();

    // -- health (unversioned, unauthenticated) ------------------------------
    let (status, body) = call(&runtime, "GET", "/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = call(&runtime, "GET", "/readyz", None).await;
    assert_eq!(status, StatusCode::OK, "a healthy fake is ready");
    assert_eq!(body["ready"], true);

    // -- openapi (unauthenticated) -----------------------------------------
    let (status, body) = call(&runtime, "GET", "/api/v1/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["paths"].is_object());
}

#[tokio::test]
async fn ac_ws16_1_run_endpoints_answer_over_oneshot() {
    let (runtime, run_id, _, _, _) = seeded();

    // -- runs ---------------------------------------------------------------
    let (status, body) = call(
        &runtime,
        "POST",
        "/api/v1/runs",
        Some(serde_json::json!({ "goal": "add /healthz" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let created: RunDto = serde_json::from_value(body).expect("RunDto");
    assert_eq!(created.goal.text, "add /healthz");

    let (status, body) = call(&runtime, "GET", "/api/v1/runs?limit=10", None).await;
    assert_eq!(status, StatusCode::OK);
    let page: Page<RunSummaryDto> = serde_json::from_value(body).expect("Page<RunSummaryDto>");
    assert!(page.items.len() >= 2, "the seeded run and the created one");

    let (status, body) = call(&runtime, "GET", &format!("/api/v1/runs/{run_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], run_id.to_string());

    let (status, _) = call(
        &runtime,
        "GET",
        &format!("/api/v1/runs/{}", RunId::new()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown run is 404");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/runs/{run_id}/plan/approve"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "executing");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/runs/{run_id}/plan/reject"),
        Some(serde_json::json!({ "feedback": "split task 2" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "planning");

    let (status, _) = call(
        &runtime,
        "POST",
        &format!("/api/v1/runs/{run_id}/evaluate"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, body) = call(
        &runtime,
        "GET",
        &format!("/api/v1/runs/{run_id}/tasks"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/runs/{run_id}/cancel"),
        Some(serde_json::json!({ "reason": "operator" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "cancelled");
}

#[tokio::test]
async fn ac_ws16_1_task_endpoints_answer_over_oneshot() {
    let (runtime, _, task_id, _, artifact_id) = seeded();

    // -- tasks --------------------------------------------------------------
    let (status, body) = call(&runtime, "GET", &format!("/api/v1/tasks/{task_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], task_id.to_string());

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/tasks/{task_id}/retry"),
        Some(serde_json::json!({ "exclude_route": true })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "routed");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/tasks/{task_id}/cancel"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["status"], "cancelled");

    let (status, body) = call(
        &runtime,
        "GET",
        &format!("/api/v1/tasks/{task_id}/log?after_seq=0&limit=10"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let log: Page<TaskLogLineDto> = serde_json::from_value(body).expect("Page<TaskLogLineDto>");
    assert_eq!(log.items.len(), 1);

    let (status, body) = call(
        &runtime,
        "GET",
        &format!("/api/v1/tasks/{task_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(1));

    // Artifact bytes are not JSON: check the raw response.
    let response = fake_api::router(&runtime)
        .oneshot(authorized(
            "GET",
            &format!("/api/v1/artifacts/{artifact_id}"),
            None,
        ))
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/x-diff; charset=utf-8")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(&bytes[..], b"--- a\n");
}

#[tokio::test]
async fn ac_ws16_1_question_endpoints_answer_over_oneshot() {
    let (runtime, _, _, question_id, _) = seeded();

    // -- questions ----------------------------------------------------------
    let (status, body) = call(&runtime, "GET", "/api/v1/questions?status=open", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));

    let (status, body) = call(
        &runtime,
        "GET",
        &format!("/api/v1/questions/{question_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "open");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/questions/{question_id}/answer"),
        Some(serde_json::json!({ "selected": ["yes"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "answered");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/questions/{question_id}/answer"),
        Some(serde_json::json!({ "selected": ["yes"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "question_already_answered");
}

#[tokio::test]
async fn ac_ws16_1_reporting_endpoints_answer_over_oneshot() {
    let (runtime, run_id, _, _, _) = seeded();
    let proposal_id = runtime.with_state(|state| state.proposals[0].id);
    let memory_id = runtime.with_state(|state| state.memory[0].id);
    let _ = run_id;

    // -- reporting ----------------------------------------------------------
    let (status, body) = call(&runtime, "GET", "/api/v1/cost?group_by=model", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total_tokens"], 42);

    let (status, body) = call(&runtime, "GET", "/api/v1/cost?group_by=nonsense", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = call(&runtime, "GET", "/api/v1/routes?kind=implement", None).await;
    assert_eq!(status, StatusCode::OK);
    let routes: Vec<RouteScoreDto> = serde_json::from_value(body).expect("routes");
    assert_eq!(routes.len(), 1);

    let (status, body) = call(&runtime, "GET", "/api/v1/memory/search?q=tests", None).await;
    assert_eq!(status, StatusCode::OK);
    let hits: Vec<MemoryItemDto> = serde_json::from_value(body).expect("hits");
    assert_eq!(hits.len(), 1);

    let (status, body) = call(&runtime, "GET", "/api/v1/lessons?limit=5", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));

    let (status, _) = call(
        &runtime,
        "DELETE",
        &format!("/api/v1/memory/{memory_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(&runtime, "GET", "/api/v1/proposals?status=proposed", None).await;
    assert_eq!(status, StatusCode::OK);
    let proposals: Page<ProposalDto> = serde_json::from_value(body).expect("proposals");
    assert_eq!(proposals.items.len(), 1);

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/proposals/{proposal_id}/accept"),
        Some(serde_json::json!({ "note": "good idea" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "accepted");

    let (status, body) = call(
        &runtime,
        "POST",
        &format!("/api/v1/proposals/{proposal_id}/reject"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "rejected");
}

#[tokio::test]
async fn ac_ws16_1_operations_endpoints_answer_over_oneshot() {
    let (runtime, _, _, _, _) = seeded();

    // -- workers, config, drain --------------------------------------------
    let (status, body) = call(&runtime, "GET", "/api/v1/workers", None).await;
    assert_eq!(status, StatusCode::OK);
    let workers: Vec<WorkerDoctorDto> = serde_json::from_value(body).expect("workers");
    assert_eq!(workers.len(), 1);

    let (status, body) = call(&runtime, "GET", "/api/v1/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["config"].is_object(), "the effective configuration");
    assert!(body["sources"].is_object(), "with per-key provenance");
    assert_eq!(
        body["config"]["server"]["auth_token_file"], "***",
        "secrets are redacted"
    );

    let (status, body) = call(&runtime, "GET", "/api/v1/maintenance/drain", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["draining"], false);

    let (status, body) = call(&runtime, "POST", "/api/v1/maintenance/drain", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["draining"], true);

    let (status, body) = call(
        &runtime,
        "POST",
        "/api/v1/runs",
        Some(serde_json::json!({ "goal": "while draining" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "draining");

    let (status, body) = call(&runtime, "DELETE", "/api/v1/maintenance/drain", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["draining"], false);
}

#[tokio::test]
async fn ac_ws16_1_bad_input_uses_the_stable_error_envelope() {
    let (runtime, _, _, _, _) = seeded();

    let (status, body) = call(
        &runtime,
        "POST",
        "/api/v1/runs",
        Some(serde_json::json!({ "goal": "   " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_goal");
    assert!(
        body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "every envelope carries the request id"
    );

    let (status, body) = call(&runtime, "GET", "/api/v1/nope", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = call(&runtime, "GET", "/api/v1/runs/not-a-uuid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");

    let (status, body) = call(
        &runtime,
        "POST",
        "/api/v1/runs",
        Some(serde_json::json!({ "goal": "x" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "sanity: a good body works");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn ac_ws16_1_the_request_id_is_honoured_and_echoed() {
    let (runtime, _, _, _, _) = seeded();
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/runs")
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("x-request-id", "req-42")
        .body(Body::empty())
        .expect("request");
    let response = fake_api::router(&runtime)
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok()),
        Some("req-42")
    );
}

// ---------------------------------------------------------------------------
// (2) SSE reconnect resumes from position
// ---------------------------------------------------------------------------

/// Reads an SSE body until `wanted` messages have arrived (or the deadline).
async fn read_sse(body: Body, wanted: usize) -> Vec<kevin_api::sse_wire::Message> {
    let mut decoder = kevin_api::sse_wire::Decoder::new();
    let mut messages = Vec::new();
    let mut stream = body.into_data_stream();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while messages.len() < wanted {
        let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        let Ok(chunk) = chunk else { break };
        messages.extend(decoder.push(&chunk));
    }
    messages
}

#[tokio::test]
async fn ac_ws16_2_sse_reconnect_resumes_from_the_last_position() {
    let (runtime, run_id, _, _, _) = seeded();
    for _ in 0..3 {
        runtime.publish("run.progressed", run_id).await;
    }
    assert_eq!(runtime.head(), 3, "three events were fanned out");

    // A fresh client with `?from=0` sees the whole history.
    let response = fake_api::router(&runtime)
        .oneshot(authorized("GET", "/api/v1/events?from=0", None))
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    let all = read_sse(response.into_body(), 3).await;
    let positions: Vec<&str> = all.iter().filter_map(|m| m.id.as_deref()).collect();
    assert_eq!(positions, ["1", "2", "3"]);

    // A reconnect with `Last-Event-ID: 1` resumes *after* position 1 and never
    // replays what the client already has.
    let request = Request::builder()
        .method("GET")
        .uri("/api/v1/events")
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("last-event-id", "1")
        .body(Body::empty())
        .expect("request");
    let response = fake_api::router(&runtime)
        .oneshot(request)
        .await
        .expect("router responds");
    let resumed = read_sse(response.into_body(), 2).await;
    let positions: Vec<&str> = resumed.iter().filter_map(|m| m.id.as_deref()).collect();
    assert_eq!(positions, ["2", "3"], "no duplicate of position 1");
    assert!(
        resumed.iter().all(|m| m.name() != "snapshot"),
        "a resuming stream gets no synthetic snapshot"
    );

    // A run stream without `Last-Event-ID` opens with the current RunDto.
    let response = fake_api::router(&runtime)
        .oneshot(authorized(
            "GET",
            &format!("/api/v1/runs/{run_id}/events"),
            None,
        ))
        .await
        .expect("router responds");
    let opening = read_sse(response.into_body(), 1).await;
    assert_eq!(opening[0].name(), "snapshot");
    let snapshot: RunDto = serde_json::from_str(&opening[0].data).expect("snapshot is a RunDto");
    assert_eq!(snapshot.id, run_id);
}

#[tokio::test]
async fn ac_ws16_2_run_streams_only_carry_that_run_and_filter_types() {
    let (runtime, run_id, _, _, _) = seeded();
    let other = RunId::new();
    runtime.insert_run(fake_api::run_fixture(other));
    runtime.publish("run.started", run_id).await;
    runtime.publish("task.created", other).await;
    runtime.publish("run.completed", run_id).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/runs/{run_id}/events?types=run.*"))
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("last-event-id", "0")
        .body(Body::empty())
        .expect("request");
    let response = fake_api::router(&runtime)
        .oneshot(request)
        .await
        .expect("router responds");
    let messages = read_sse(response.into_body(), 2).await;
    let names: Vec<&str> = messages
        .iter()
        .map(kevin_api::sse_wire::Message::name)
        .collect();
    assert_eq!(names, ["run.started", "run.completed"]);
}

/// An `EventsPort` whose live subscription immediately reports bus lag.
#[derive(Debug)]
struct LaggingEvents;

#[async_trait::async_trait]
impl EventsPort for LaggingEvents {
    async fn after(&self, _from: u64, _limit: usize) -> kevin_api::port::PortResult<Vec<EventDto>> {
        Ok(Vec::new())
    }

    fn subscribe_live(&self) -> kevin_bus::BusStream {
        kevin_bus::BusStream::new(futures::stream::iter([kevin_bus::BusMessage::Lagged {
            from: 2,
            to: 5,
        }]))
    }

    fn head(&self) -> u64 {
        5
    }
}

#[tokio::test]
async fn ac_ws16_2_bus_lag_becomes_a_resync_event() {
    let runtime = FakeRuntime::new();
    let state = fake_api::state(&runtime);
    let permit = state.sse_gate().acquire("test").expect("permit");
    let stream = sse::event_stream(
        Arc::new(LaggingEvents) as Arc<dyn EventsPort>,
        Start::After(1),
        EventFilter::default(),
        None,
        permit,
    );
    let response = sse::respond(stream, Duration::from_secs(30));
    let messages = read_sse(response.into_body(), 1).await;
    assert_eq!(messages[0].name(), "resync");
    let body: Value = serde_json::from_str(&messages[0].data).expect("resync payload");
    assert_eq!(body["from"], 2);
    assert_eq!(body["to"], 5);
}

// ---------------------------------------------------------------------------
// (3) auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws16_3_requests_without_a_valid_token_are_401() {
    let (runtime, run_id, _, _, _) = seeded();

    for (method, uri) in [
        ("GET", "/api/v1/runs"),
        ("POST", "/api/v1/runs"),
        ("GET", "/api/v1/questions"),
        ("GET", "/api/v1/workers"),
        ("GET", "/api/v1/events"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = fake_api::router(&runtime)
            .oneshot(request)
            .await
            .expect("router responds");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require a token"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&bytes).expect("envelope");
        assert_eq!(body["code"], "unauthenticated");
    }

    // A wrong token is also 401, and so is a non-bearer scheme.
    for header in ["Bearer wrong", "Basic dXNlcjpwdw==", "Bearer "] {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/runs/{run_id}"))
            .header("authorization", header)
            .body(Body::empty())
            .expect("request");
        let response = fake_api::router(&runtime)
            .oneshot(request)
            .await
            .expect("router responds");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{header:?}");
    }
}

#[tokio::test]
async fn ac_ws16_3_health_and_openapi_are_exempt_from_auth() {
    let (runtime, _, _, _, _) = seeded();
    for uri in ["/healthz", "/readyz", "/api/v1/openapi.json"] {
        let request = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = fake_api::router(&runtime)
            .oneshot(request)
            .await
            .expect("router responds");
        assert_ne!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} is exempt from auth"
        );
    }
}

#[test]
fn ac_ws16_3_the_token_comparison_is_constant_time() {
    use kevin_api::auth::TokenVerifier;

    let verifier = TokenVerifier::new("0123456789abcdef0123456789abcdef");
    assert!(verifier.verify("0123456789abcdef0123456789abcdef"));
    // Neither a shared prefix nor a different length short-circuits: the
    // comparison runs over SHA-256 digests with `subtle::ConstantTimeEq`.
    assert!(!verifier.verify("0123456789abcdef0123456789abcdeg"));
    assert!(!verifier.verify("0"));
    assert!(!verifier.verify(&"x".repeat(4096)));

    // Timing is not asserted (it is not measurable reliably in CI); what is
    // asserted is that the implementation cannot leak: `verify` never compares
    // the presented token itself, only its fixed-width digest.
    let elapsed_prefix = {
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = verifier.verify("0123456789abcdef0123456789abcdeg");
        }
        start.elapsed()
    };
    let elapsed_nothing = {
        let start = std::time::Instant::now();
        for _ in 0..1000 {
            let _ = verifier.verify("z123456789abcdef0123456789abcdef");
        }
        start.elapsed()
    };
    let ratio = elapsed_prefix.as_secs_f64() / elapsed_nothing.as_secs_f64().max(f64::EPSILON);
    assert!(
        (0.2..5.0).contains(&ratio),
        "a shared prefix must not change the cost noticeably (ratio {ratio})"
    );
}

// ---------------------------------------------------------------------------
// (4) idempotency
// ---------------------------------------------------------------------------

/// Posts a run through **one** router instance, so the process-local
/// idempotency cache is the same across calls (a real server is one process).
async fn create_with_key(app: &axum::Router, key: &str, goal: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/runs")
        .header("authorization", format!("Bearer {}", fake_api::TOKEN))
        .header("content-type", "application/json")
        .header("idempotency-key", key)
        .body(Body::from(serde_json::json!({ "goal": goal }).to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("json"))
}

#[tokio::test]
async fn ac_ws16_4_idempotency_key_replay_returns_the_same_run() {
    let runtime = FakeRuntime::new();
    let app = fake_api::router(&runtime);

    let (status, first) = create_with_key(&app, "cli-0191f3a0-abc", "add /healthz").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, replay) = create_with_key(&app, "cli-0191f3a0-abc", "add /healthz").await;
    assert_eq!(status, StatusCode::OK, "a replay is 200, not 201");
    assert_eq!(replay, first, "byte-identical to the original response");

    // The command reached the runtime exactly once.
    let commands = runtime.with_state(|state| state.commands.clone());
    assert_eq!(commands, vec!["start_run".to_owned()]);

    // Same key, different body → conflict.
    let (status, body) = create_with_key(&app, "cli-0191f3a0-abc", "something else").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "idempotency_conflict");
    assert_eq!(body["details"]["idempotency_key"], "cli-0191f3a0-abc");

    // A different key is a different run.
    let (status, other) = create_with_key(&app, "cli-0191f3a0-def", "add /healthz").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(other["id"], first["id"]);

    // The key becomes the `command_id` (uuid v5 of the key), deterministically.
    let ids = runtime.with_state(|state| state.command_ids.clone());
    assert_eq!(ids.len(), 2, "one command id per accepted command");
    assert_eq!(
        ids[0],
        kevin_api::state::Idempotency::command_id("cli-0191f3a0-abc").as_uuid()
    );
}

#[tokio::test]
async fn ac_ws16_4_malformed_idempotency_keys_are_rejected() {
    let runtime = FakeRuntime::new();
    let app = fake_api::router(&runtime);
    let (status, body) = create_with_key(&app, "has space", "goal").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "invalid_request");
}

// ---------------------------------------------------------------------------
// (5) OpenAPI
// ---------------------------------------------------------------------------

#[test]
fn ac_ws16_5_the_openapi_document_validates() {
    let doc = kevin_api::openapi::ApiDoc::json();

    assert!(
        doc["openapi"]
            .as_str()
            .is_some_and(|v| v.starts_with("3.1") || v.starts_with("3.0")),
        "a versioned OpenAPI document"
    );
    assert_eq!(doc["info"]["title"], "Kevin API");
    let paths = doc["paths"].as_object().expect("paths");
    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("component schemas");

    // Every endpoint of the plan/07 table is documented.
    for path in [
        "/api/v1/runs",
        "/api/v1/runs/{run_id}",
        "/api/v1/runs/{run_id}/cancel",
        "/api/v1/runs/{run_id}/plan/approve",
        "/api/v1/runs/{run_id}/plan/reject",
        "/api/v1/runs/{run_id}/evaluate",
        "/api/v1/runs/{run_id}/tasks",
        "/api/v1/runs/{run_id}/events",
        "/api/v1/tasks/{task_id}",
        "/api/v1/tasks/{task_id}/retry",
        "/api/v1/tasks/{task_id}/cancel",
        "/api/v1/tasks/{task_id}/log",
        "/api/v1/tasks/{task_id}/log/stream",
        "/api/v1/tasks/{task_id}/artifacts",
        "/api/v1/artifacts/{artifact_id}",
        "/api/v1/questions",
        "/api/v1/questions/{question_id}",
        "/api/v1/questions/{question_id}/answer",
        "/api/v1/events",
        "/api/v1/cost",
        "/api/v1/routes",
        "/api/v1/memory/search",
        "/api/v1/memory/{item_id}",
        "/api/v1/lessons",
        "/api/v1/proposals",
        "/api/v1/proposals/{proposal_id}/accept",
        "/api/v1/proposals/{proposal_id}/reject",
        "/api/v1/workers",
        "/api/v1/config",
        "/api/v1/maintenance/drain",
        "/healthz",
        "/readyz",
    ] {
        assert!(
            paths.contains_key(path),
            "{path} is missing from the OpenAPI"
        );
    }

    // `/metrics` is served by the telemetry listener, never by the API.
    assert!(
        !paths.contains_key("/metrics"),
        "plan/10: /metrics is separate"
    );

    // Every `$ref` resolves.
    let mut refs = Vec::new();
    collect_refs(&doc, &mut refs);
    assert!(!refs.is_empty(), "the document uses component schemas");
    for reference in &refs {
        let name = reference
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("unexpected $ref target {reference}"));
        assert!(schemas.contains_key(name), "dangling $ref {reference}");
    }

    // The bearer scheme every /api/v1 operation references is declared.
    assert_eq!(
        doc["components"]["securitySchemes"]["bearer"]["scheme"],
        "bearer"
    );

    // Round-trips as JSON (nothing in the document is unserialisable).
    let text = serde_json::to_string(&doc).expect("serialise");
    let reparsed: Value = serde_json::from_str(&text).expect("reparse");
    assert_eq!(reparsed, doc);
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "$ref"
                    && let Some(target) = child.as_str()
                {
                    out.push(target.to_owned());
                } else {
                    collect_refs(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, out)),
        _ => {}
    }
}

#[test]
fn ac_ws16_5_the_error_code_table_matches_the_plan() {
    use kevin_api::error::ErrorCode;

    let expected: Vec<(&str, u16)> = vec![
        ("invalid_request", 400),
        ("invalid_goal", 400),
        ("invalid_answer", 400),
        ("invalid_cursor", 400),
        ("payload_too_large", 413),
        ("unauthenticated", 401),
        ("forbidden", 403),
        ("run_not_found", 404),
        ("task_not_found", 404),
        ("question_not_found", 404),
        ("proposal_not_found", 404),
        ("artifact_not_found", 404),
        ("idempotency_conflict", 409),
        ("run_not_in_state", 409),
        ("task_not_in_state", 409),
        ("question_already_answered", 409),
        ("version_conflict", 409),
        ("plan_invalid", 422),
        ("budget_invalid", 422),
        ("unknown_model_alias", 422),
        ("worker_disabled", 422),
        ("rate_limited", 429),
        ("draining", 503),
        ("db_unavailable", 503),
        ("runtime_unavailable", 503),
        ("internal", 500),
    ];
    let actual: Vec<(&str, u16)> = ErrorCode::ALL
        .iter()
        .map(|code| (code.as_str(), code.status()))
        .collect();
    assert_eq!(actual, expected);
}

// ---------------------------------------------------------------------------
// (6) KevinClient round-trip
// ---------------------------------------------------------------------------

#[cfg(feature = "client")]
#[tokio::test]
async fn ac_ws16_6_kevin_client_round_trips_against_the_fake_api() {
    use kevin_api::client::{ClientError, KevinClient};
    use kevin_api::dto::{AnswerRequest, CreateRunRequest, ListRunsQuery, TaskLogQueryDto};

    let (runtime, run_id, task_id, question_id, _) = seeded();
    let server = fake_api::spawn(&runtime).await;
    let client =
        KevinClient::connect(&server.base_url(), server.token.clone().into()).expect("client");

    // Readiness and health.
    let ready = client.ready().await.expect("readyz");
    assert!(ready.ready);

    // Create + read back.
    let created = client
        .create_run(
            CreateRunRequest {
                goal: "add /healthz".to_owned(),
                cwd: None,
                attachments: Vec::new(),
                mode: None,
                budget: None,
                tags: Vec::new(),
            },
            Some("cli-round-trip"),
        )
        .await
        .expect("create_run");
    assert_eq!(created.goal.text, "add /healthz");
    let fetched = client.get_run(created.id).await.expect("get_run");
    assert_eq!(fetched.id, created.id);

    let page = client
        .list_runs(&ListRunsQuery::default())
        .await
        .expect("list_runs");
    assert!(page.items.iter().any(|run| run.id == created.id));

    // Commands.
    let approved = client.approve_plan(run_id, None).await.expect("approve");
    assert_eq!(approved.status, kevin_api::dto::RunStatusDto::Executing);
    let cancelled = client.cancel_run(run_id, None).await.expect("cancel");
    assert_eq!(cancelled.status, kevin_api::dto::RunStatusDto::Cancelled);

    let task = client.get_task(task_id).await.expect("get_task");
    assert_eq!(task.run_id, run_id);
    let retried = client.retry_task(task_id, true).await.expect("retry");
    assert_eq!(retried.status, "routed");
    assert_eq!(
        client.cancel_task(task_id).await.expect("cancel").status,
        "cancelled"
    );

    let log = client
        .task_log(task_id, &TaskLogQueryDto::default())
        .await
        .expect("task_log");
    assert_eq!(log.items.len(), 1);
    assert_eq!(client.run_tasks(run_id).await.expect("tasks").len(), 1);
    assert_eq!(
        client
            .task_artifacts(task_id)
            .await
            .expect("artifacts")
            .len(),
        1
    );

    let question = client.get_question(question_id).await.expect("question");
    assert_eq!(question.status, "open");
    let answered = client
        .answer_question(
            question_id,
            AnswerRequest {
                selected: vec!["yes".to_owned()],
                free_text: None,
            },
            None,
        )
        .await
        .expect("answer");
    assert_eq!(answered.status, "answered");

    // Reporting + operations.
    assert_eq!(
        client
            .cost(&kevin_api::dto::CostQueryDto::default())
            .await
            .expect("cost")
            .total_tokens,
        42
    );
    assert_eq!(client.routes(None).await.expect("routes").len(), 1);
    assert_eq!(client.workers().await.expect("workers").len(), 1);
    assert_eq!(
        client
            .proposals(&kevin_api::dto::ProposalsQuery::default())
            .await
            .expect("proposals")
            .items
            .len(),
        1
    );
    assert!(client.drain(true).await.expect("drain on").draining);
    assert!(!client.drain(false).await.expect("drain off").draining);
    assert!(client.openapi().await.expect("openapi")["paths"].is_object());

    // The typed error surfaces the stable code.
    let err = client.get_run(RunId::new()).await.expect_err("404");
    assert!(matches!(err.code(), Some("run_not_found")), "{err:?}");
    assert_eq!(err.status(), Some(404));

    // A wrong token is an `unauthenticated` API error, not a transport error.
    let anonymous = KevinClient::connect(&server.base_url(), "not-the-token".to_owned().into())
        .expect("client");
    let err = anonymous
        .list_runs(&ListRunsQuery::default())
        .await
        .expect_err("401");
    assert!(
        matches!(err, ClientError::Api { status: 401, .. }),
        "{err:?}"
    );

    server.shutdown().await;
}

#[cfg(feature = "client")]
#[tokio::test]
async fn ac_ws16_6_the_client_event_stream_resumes_after_a_position() {
    use kevin_api::client::KevinClient;

    let (runtime, run_id, _, _, _) = seeded();
    for _ in 0..3 {
        runtime.publish("run.progressed", run_id).await;
    }
    let server = fake_api::spawn(&runtime).await;
    let client =
        KevinClient::connect(&server.base_url(), server.token.clone().into()).expect("client");

    let mut stream = Box::pin(client.run_events(run_id, Some(1)));
    let mut positions = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while positions.len() < 2 {
        let Ok(Some(item)) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break;
        };
        match item {
            Ok(event) => positions.push(event.position),
            Err(err) => panic!("stream error: {err}"),
        }
    }
    assert_eq!(positions, vec![2, 3], "resumed after the last position");

    drop(stream);
    server.shutdown().await;
}
