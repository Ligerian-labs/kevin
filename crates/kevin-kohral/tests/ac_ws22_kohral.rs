//! WS-22 acceptance tests (`plan/12-workstreams.md`, `plan/08-kohral-runtime.md`).
//!
//! Every test boots a **real** Kevin — its own Postgres database, the real
//! event store, the real orchestrator and roles, the in-process `fake` worker
//! with the two conformance hooks — and talks to it over HTTP exactly like
//! Kohral does. Nothing here mocks the contract.
//!
//! | Test | Acceptance criterion |
//! |---|---|
//! | `ac_ws22_1_contract_basic` | `contract.py --runtime hermes basic` passes |
//! | `ac_ws22_2_contract_crash_phases` | `accept-crash` → kill → `verify-crash` passes |
//! | `ac_ws22_3_idempotency_semantics` | 202 / 200 / 409 on `Idempotency-Key` |
//! | `ac_ws22_4_partial_output_seq_monotonic` | append-only output, monotonic `seq` |
//! | `ac_ws22_5_bad_token_rejected` | 401 (or 403) everywhere but `/health` |
//! | `ac_ws22_6_catalog_derived_from_aliases` | the catalog mirrors `[models.*]` |
//!
//! Tests 1 and 2 need Kohral's `contract.py`. It is **not** vendored: a copy of
//! somebody else's contract silently stops being their contract. When the
//! checkout is missing the two tests skip and the ported assertions in tests
//! 3–6 still cover the same ground, so `just ci` is green on a laptop without
//! Kohral and strict in CI, which clones it.

use std::time::Duration;

use kevin_kohral::conformance::{ContractScript, Gateway, Phase, run_suite};
use kevin_kohral::projection::Narrative;
use kevin_testkit::pg::TestDb;
use serde_json::{Value, json};

/// The token the gateway is started with.
const TOKEN: &str = "conformance-token";

/// `contract.py`'s own deadline for a turn to terminalise.
const STATE_TIMEOUT: Duration = Duration::from_secs(90);

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("http client")
}

/// The turn body `HermesRuntimeStrategy::submitTurn` sends.
fn turn(input: &str) -> Value {
    json!({
        "input": input,
        "instructions": "",
        "conversation_history": [],
        "session_id": "conformance",
        "model": "hermes-agent",
        "attachments": [],
    })
}

async fn submit(gateway: &Gateway, key: &str, input: &str) -> (reqwest::StatusCode, Value) {
    let response = client()
        .post(format!("{}/v1/runs", gateway.base_url()))
        .bearer_auth(gateway.token())
        .header("Idempotency-Key", key)
        .header("X-Hermes-Session-Key", "kohral:conformance")
        .json(&turn(input))
        .send()
        .await
        .expect("POST /v1/runs");
    let status = response.status();
    let body = response.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

async fn status_of(gateway: &Gateway, run_id: &str) -> Value {
    client()
        .get(format!("{}/v1/runs/{run_id}", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET /v1/runs/{id}")
        .json::<Value>()
        .await
        .expect("status body")
}

/// `POST /v1/runs/{id}/stop`.
async fn stop(gateway: &Gateway, run_id: &str) -> Value {
    client()
        .post(format!("{}/v1/runs/{run_id}/stop", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("POST stop")
        .json::<Value>()
        .await
        .expect("stop body")
}

/// Polls until the turn is terminal, returning every snapshot it saw.
async fn poll_until_terminal(gateway: &Gateway, run_id: &str) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + STATE_TIMEOUT;
    let mut snapshots = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let snapshot = status_of(gateway, run_id).await;
        let terminal = matches!(
            snapshot["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        );
        snapshots.push(snapshot);
        if terminal {
            return snapshots;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!(
        "the turn never terminalised; last snapshot: {:?}",
        snapshots.last()
    );
}

// ---------------------------------------------------------------------------
// 1 & 2 — Kohral's own conformance script
// ---------------------------------------------------------------------------

/// `contract.py basic --runtime hermes` against a real Kevin with the fake
/// worker: capabilities, model catalog, 401 on a wrong token, submit → retry →
/// conflict, and a terminal `completed` whose output is exactly `kohral-ok`.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_1_contract_basic() {
    kevin_testkit::skip_unless_pg!();
    let Some(script) = ContractScript::locate() else {
        eprintln!(
            "skipping ac_ws22_1: Kohral's contract.py was not found \
             (set {} to run it)",
            kevin_kohral::conformance::SCRIPT_ENV
        );
        return;
    };
    let db = TestDb::new().await;
    let mut gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    let reports = run_suite(&script, &mut gateway, &[Phase::Basic])
        .await
        .expect("contract.py basic");
    assert!(reports["basic"].success);
    gateway.shutdown().await;
}

/// The crash phases: submit a turn that never finishes, kill the runtime
/// without recording anything, boot again, and see the turn terminalised as
/// `failed / runtime_restarted` — never replayed.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_2_contract_crash_phases() {
    kevin_testkit::skip_unless_pg!();
    let Some(script) = ContractScript::locate() else {
        eprintln!(
            "skipping ac_ws22_2: Kohral's contract.py was not found \
             (set {} to run it)",
            kevin_kohral::conformance::SCRIPT_ENV
        );
        return;
    };
    let db = TestDb::new().await;
    let mut gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    let reports = run_suite(
        &script,
        &mut gateway,
        &[Phase::AcceptCrash, Phase::VerifyCrash],
    )
    .await
    .expect("contract.py crash phases");
    assert!(reports["accept-crash"].success);
    assert!(reports["verify-crash"].success);
    gateway.shutdown().await;
}

/// The same crash contract, asserted directly, so the guarantee is covered
/// even where Kohral is not checked out.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_2_a_restart_terminalises_the_turn_as_runtime_restarted() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let mut gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let key = format!("crash-{}", uuid::Uuid::new_v4());
    let (status, body) = submit(&gateway, &key, "[[KOHRAL_HOLD]]").await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body:?}");
    let run_id = body["run_id"].as_str().expect("run id").to_owned();

    // The turn must be accepted and *not* finish on its own.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let held = status_of(&gateway, &run_id).await;
    assert!(
        matches!(held["status"].as_str(), Some("queued" | "running")),
        "the held turn must stay non-terminal: {held:?}"
    );

    gateway.crash().await;
    gateway.restart().await.expect("restart");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready after restart");

    let after = status_of(&gateway, &run_id).await;
    assert_eq!(after["status"], "failed", "{after:?}");
    assert_eq!(after["error_code"], "runtime_restarted", "{after:?}");
    assert!(
        after["seq"].as_i64().unwrap_or_default() > held["seq"].as_i64().unwrap_or_default(),
        "the terminal transition advances seq: {held:?} → {after:?}"
    );
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3 — idempotency
// ---------------------------------------------------------------------------

/// `plan/08` §1.2: a new key is `202`, the same key with the same request is
/// `200` with the *same* run id, and the same key with a different request is
/// `409 idempotency_conflict`.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_3_idempotency_semantics() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let key = format!("turn-{}", uuid::Uuid::new_v4());
    let (first_status, first) = submit(&gateway, &key, "reply deterministically").await;
    assert_eq!(first_status, reqwest::StatusCode::ACCEPTED, "{first:?}");
    let run_id = first["run_id"].as_str().expect("run id").to_owned();
    assert_eq!(first["status"], "queued");

    let (retry_status, retry) = submit(&gateway, &key, "reply deterministically").await;
    assert_eq!(retry_status, reqwest::StatusCode::OK, "{retry:?}");
    assert_eq!(retry["run_id"].as_str(), Some(run_id.as_str()));

    let (conflict_status, conflict) = submit(&gateway, &key, "a different request").await;
    assert_eq!(
        conflict_status,
        reqwest::StatusCode::CONFLICT,
        "{conflict:?}"
    );
    assert_eq!(conflict["code"], "idempotency_conflict");
    assert_eq!(conflict["error"]["code"], "idempotency_conflict");

    // A malformed key never reaches the ledger.
    let bad = client()
        .post(format!("{}/v1/runs", gateway.base_url()))
        .bearer_auth(gateway.token())
        .header("Idempotency-Key", "not a key")
        .json(&turn("hi"))
        .send()
        .await
        .expect("POST with a bad key");
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        bad.json::<Value>().await.expect("body")["code"],
        "invalid_idempotency_key"
    );

    // And the turn still runs to completion with the deterministic output.
    let snapshots = poll_until_terminal(&gateway, &run_id).await;
    let terminal = snapshots.last().expect("a terminal snapshot");
    assert_eq!(terminal["status"], "completed", "{terminal:?}");
    assert_eq!(
        terminal["output"], "kohral-ok",
        "the conformance hook must produce exactly `kohral-ok`: {terminal:?}"
    );
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4 — turn invariants
// ---------------------------------------------------------------------------

/// Kohral's turn invariants (`kohral docs/10-conversations.md`): `seq` never
/// goes backwards and `partial_output` is append-only. Run with the **full**
/// narrative, which is the mode that actually appends while the turn runs.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_4_partial_output_seq_monotonic() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start_with(db.pool().clone(), TOKEN, Narrative::Full)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let key = format!("turn-{}", uuid::Uuid::new_v4());
    let (status, body) = submit(&gateway, &key, "reply deterministically").await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body:?}");
    let run_id = body["run_id"].as_str().expect("run id").to_owned();

    let snapshots = poll_until_terminal(&gateway, &run_id).await;
    let mut previous_seq = -1i64;
    let mut previous_output = String::new();
    for snapshot in &snapshots {
        let seq = snapshot["seq"].as_i64().expect("seq");
        let output = snapshot["partial_output"].as_str().expect("partial_output");
        assert!(
            seq >= previous_seq,
            "seq went backwards: {previous_seq} → {seq}"
        );
        assert!(
            output.starts_with(&previous_output),
            "the streamed prefix was rewritten:\n{previous_output:?}\n{output:?}"
        );
        previous_seq = seq;
        previous_output = output.to_owned();
    }

    let terminal = snapshots.last().expect("a terminal snapshot");
    assert_eq!(terminal["status"], "completed", "{terminal:?}");
    assert!(
        terminal["seq"].as_i64().expect("seq") > 1,
        "a narrated turn appends more than once: {terminal:?}"
    );
    assert_eq!(
        terminal["output"], terminal["partial_output"],
        "`output` is the final `partial_output`"
    );
    assert!(
        terminal["partial_output"]
            .as_str()
            .expect("partial_output")
            .contains("kohral-ok"),
        "the answer is appended, never replacing the narrative: {terminal:?}"
    );

    // The assistant message is stable and mirrors the final output.
    let messages = client()
        .get(format!(
            "{}/api/sessions/conformance/messages",
            gateway.base_url()
        ))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET messages")
        .json::<Value>()
        .await
        .expect("messages body");
    let rows = messages["messages"].as_array().expect("messages array");
    assert_eq!(
        rows.len(),
        2,
        "one user and one assistant message: {rows:?}"
    );
    assert!(rows.iter().any(|row| row["role"] == "user"));
    let assistant = rows
        .iter()
        .find(|row| row["role"] == "assistant")
        .expect("an assistant message");
    assert_eq!(assistant["id"], terminal["message_id"]);
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5 — auth
// ---------------------------------------------------------------------------

/// `plan/08` §1.1: every route but `/health` needs the Kohral bearer token,
/// and a wrong one is `401` with the Hermes error envelope (the conformance
/// script accepts `401` or `403`).
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_5_bad_token_rejected() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    for path in [
        "/v1/capabilities",
        "/v1/kohral/models",
        "/health/detailed",
        "/api/sessions",
        "/v1/maintenance/drain",
    ] {
        let response = client()
            .get(format!("{}{path}", gateway.base_url()))
            .bearer_auth(format!("{TOKEN}-wrong"))
            .send()
            .await
            .expect("GET with a wrong token");
        assert!(
            matches!(response.status().as_u16(), 401 | 403),
            "{path} answered {} with a wrong token",
            response.status()
        );
        let body = response.json::<Value>().await.expect("error body");
        assert_eq!(body["code"], "invalid_api_key", "{path}: {body:?}");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "Invalid API key");
    }

    // No token at all is refused the same way.
    let anonymous = client()
        .get(format!("{}/v1/capabilities", gateway.base_url()))
        .send()
        .await
        .expect("GET without a token");
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Submitting a turn with a wrong token must not create a run either.
    let submitted = client()
        .post(format!("{}/v1/runs", gateway.base_url()))
        .bearer_auth("nope")
        .header("Idempotency-Key", "turn-unauthorised")
        .json(&turn("reply deterministically"))
        .send()
        .await
        .expect("POST with a wrong token");
    assert_eq!(submitted.status(), reqwest::StatusCode::UNAUTHORIZED);

    // `/health` stays open: Kohral polls it without secrets.
    let health = client()
        .get(format!("{}/health", gateway.base_url()))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health.status(), reqwest::StatusCode::OK);
    let body = health.json::<Value>().await.expect("health body");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["platform"], "kevin");
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6 — model catalog
// ---------------------------------------------------------------------------

/// `plan/08` §1.5: the catalog is derived from `[models.*]`, in the exact v1
/// envelope `ModelCatalog::fetchRuntimeCatalog` accepts.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_6_catalog_derived_from_aliases() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let catalog = client()
        .get(format!("{}/v1/kohral/models", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET /v1/kohral/models")
        .json::<Value>()
        .await
        .expect("catalog body");

    assert_eq!(catalog["object"], "kohral.runtime_model_catalog");
    assert_eq!(catalog["version"], 1);
    let providers = catalog["providers"].as_array().expect("providers array");

    // The conformance profile configures exactly one alias, `fake`, on the
    // in-process worker — so exactly one provider with one model appears.
    assert_eq!(providers.len(), 1, "{providers:?}");
    let models = providers[0]["models"].as_array().expect("models array");
    assert_eq!(models.len(), 1, "{models:?}");
    assert_eq!(providers[0]["id"], "fake");
    assert_eq!(models[0]["id"], "fake");
    assert_eq!(models[0]["name"], "fake");
    assert!(
        models[0]["capabilities"]
            .as_array()
            .expect("capabilities")
            .iter()
            .any(|c| c == "reasoning"),
        "the balanced tier reasons: {:?}",
        models[0]
    );

    // Capabilities and the catalog agree that v1 is served.
    let capabilities = client()
        .get(format!("{}/v1/capabilities", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET /v1/capabilities")
        .json::<Value>()
        .await
        .expect("capabilities body");
    assert_eq!(capabilities["features"]["runtime_model_catalog_v1"], true);
    assert_eq!(capabilities["model"], "fake");

    // An unknown override is rejected before a run is created.
    let unknown = client()
        .post(format!("{}/v1/runs", gateway.base_url()))
        .bearer_auth(gateway.token())
        .header("Idempotency-Key", "turn-unknown-model")
        .json(&json!({
            "input": "hello",
            "instructions": "",
            "conversation_history": [],
            "session_id": "conformance",
            "model": "anthropic/claude-opus-5",
        }))
        .send()
        .await
        .expect("POST with an unknown model");
    assert_eq!(unknown.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        unknown.json::<Value>().await.expect("body")["code"],
        "unknown_model"
    );
    gateway.shutdown().await;
}

// ---------------------------------------------------------------------------
// Drain, stop and sessions
// ---------------------------------------------------------------------------

/// `plan/08` §1.7: `POST` closes admission (`503 gateway_draining` for a new
/// key, but a known key still resolves), `GET` reports, `DELETE` reopens. The
/// payload carries the `accepting` / `activeWork` pair
/// `HermesRuntimeStrategy::parseDrainState` insists on.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_7_drain_closes_admission_without_orphaning_known_keys() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let key = format!("turn-{}", uuid::Uuid::new_v4());
    let (status, accepted) = submit(&gateway, &key, "reply deterministically").await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{accepted:?}");

    let drain = client()
        .post(format!("{}/v1/maintenance/drain", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("POST drain")
        .json::<Value>()
        .await
        .expect("drain body");
    assert_eq!(drain["accepting"], false);
    assert_eq!(drain["draining"], true);
    assert!(drain["activeWork"].is_i64(), "{drain:?}");

    // A key Kevin already promised to run still answers.
    let (replay_status, replay) = submit(&gateway, &key, "reply deterministically").await;
    assert_eq!(replay_status, reqwest::StatusCode::OK, "{replay:?}");
    assert_eq!(replay["run_id"], accepted["run_id"]);

    // A new key does not.
    let (refused_status, refused) = submit(
        &gateway,
        &format!("turn-{}", uuid::Uuid::new_v4()),
        "reply deterministically",
    )
    .await;
    assert_eq!(
        refused_status,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "{refused:?}"
    );
    assert_eq!(refused["code"], "gateway_draining");

    let resumed = client()
        .delete(format!("{}/v1/maintenance/drain", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("DELETE drain")
        .json::<Value>()
        .await
        .expect("drain body");
    assert_eq!(resumed["accepting"], true);
    gateway.shutdown().await;
}

/// `POST /v1/runs/{id}/stop` is idempotent and never resurrects a terminal
/// turn; the sessions endpoints report the conversation Kohral polls.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws22_8_stop_is_idempotent_and_sessions_are_listed() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let gateway = Gateway::start(db.pool().clone(), TOKEN)
        .await
        .expect("gateway");
    gateway
        .wait_ready(Duration::from_secs(30))
        .await
        .expect("ready");

    let key = format!("turn-{}", uuid::Uuid::new_v4());
    let (_, accepted) = submit(&gateway, &key, "[[KOHRAL_HOLD]]").await;
    let run_id = accepted["run_id"].as_str().expect("run id").to_owned();

    let first = stop(&gateway, &run_id).await;
    assert!(
        matches!(
            first["status"].as_str(),
            Some("stopping" | "cancelled" | "failed")
        ),
        "{first:?}"
    );
    let second = stop(&gateway, &run_id).await;
    assert_eq!(
        second["run_id"].as_str(),
        Some(run_id.as_str()),
        "{second:?}"
    );

    let snapshots = poll_until_terminal(&gateway, &run_id).await;
    let terminal = snapshots.last().expect("terminal");
    assert_eq!(terminal["status"], "cancelled", "{terminal:?}");

    // Stopping a terminal turn is a no-op that reports the same state.
    let third = stop(&gateway, &run_id).await;
    assert_eq!(third["status"], "cancelled", "{third:?}");

    let sessions = client()
        .get(format!("{}/api/sessions", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET sessions")
        .json::<Value>()
        .await
        .expect("sessions body");
    let rows = sessions["sessions"].as_array().expect("sessions array");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], "conformance");
    assert_eq!(rows[0]["session_id"], "conformance");
    assert_eq!(sessions["data"], sessions["sessions"]);

    let one = client()
        .get(format!("{}/api/sessions/conformance", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET session")
        .json::<Value>()
        .await
        .expect("session body");
    assert_eq!(one["id"], "conformance");
    assert_eq!(one["runs"].as_array().expect("runs").len(), 1);

    let missing = client()
        .get(format!("{}/api/sessions/nope", gateway.base_url()))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET missing session");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    let unknown_run = client()
        .get(format!(
            "{}/v1/runs/{}",
            gateway.base_url(),
            uuid::Uuid::new_v4()
        ))
        .bearer_auth(gateway.token())
        .send()
        .await
        .expect("GET unknown run");
    assert_eq!(unknown_run.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        unknown_run.json::<Value>().await.expect("body")["code"],
        "run_not_found"
    );
    gateway.shutdown().await;
}
