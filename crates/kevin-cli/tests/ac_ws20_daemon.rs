//! WS-20 acceptance criteria (`plan/12-workstreams.md`): `kevin serve` as a
//! daemon, and the operations around it.
//!
//! 1. SIGTERM drains within `kevin.shutdown_grace_period` and the attempt that
//!    was running is terminalised.
//! 2. `/readyz` is false while draining and while the database is down;
//!    `/healthz` stays true through both.
//! 3. A restart terminalises the attempts the dead process left behind as
//!    `runtime_restarted`.
//! 4. `telemetry.metrics_bind` exposes the metric names of
//!    `plan/10-observability-ops.md` — and every documented name is emitted
//!    somewhere in the production sources.
//! 5. A client attaches with the bearer token, and
//!    `kevin config rotate-token` + `SIGHUP` rotates it with no downtime.
//!
//! Every scenario runs the real binary against a per-test database
//! (`kevin_testkit::pg::TestDb`) with the in-process `fake` worker and drives
//! real unix signals. No coding-agent CLI is ever spawned.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Harness, SCENARIO, SCENARIO_HOLDING, run_events};
use kevin_api::dto::CreateRunRequest;

/// `auto_approve_plans` so a run reaches `executing` without a human.
const AUTO_APPROVE: &str = "[kevin]\nauto_approve_plans = true\n";

/// Same, plus a Prometheus listener on an ephemeral port.
const AUTO_APPROVE_WITH_METRICS: &str =
    "[kevin]\nauto_approve_plans = true\n\n[telemetry]\nmetrics_bind = \"127.0.0.1:0\"\n";

/// Longest a scenario waits for an event to show up in `core.events`.
const PATIENCE: Duration = Duration::from_secs(60);

fn goal(text: &str, cwd: &Path) -> CreateRunRequest {
    CreateRunRequest {
        goal: text.to_owned(),
        cwd: Some(cwd.to_path_buf()),
        attachments: Vec::new(),
        mode: None,
        budget: None,
        tags: Vec::new(),
    }
}

/// Polls `core.events` until `event_type` appears.
async fn await_event(harness: &Harness, event_type: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        if run_events(harness)
            .await
            .iter()
            .any(|(kind, _)| kind == event_type)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let seen: Vec<String> = run_events(harness)
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    panic!("{event_type} never happened; saw {seen:?}");
}

/// Every `task.attempt_failed` payload recorded so far.
async fn attempt_failures(harness: &Harness) -> Vec<serde_json::Value> {
    run_events(harness)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "task.attempt_failed")
        .map(|(_, payload)| payload)
        .collect()
}

// ---------------------------------------------------------------------------
// 1 — SIGTERM drains within the grace period
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws20_1_sigterm_drains_within_grace_and_terminalises_attempts() {
    kevin_testkit::skip_unless_pg!();
    // The implement task holds until it is cancelled, so a SIGTERM always
    // lands with exactly one attempt in flight.
    let harness = Harness::with_scenario_and_extra(SCENARIO_HOLDING, AUTO_APPROVE).await;
    let daemon = harness.serve(&[]).await;
    let client = daemon.client(harness.token());

    client
        .create_run(goal("hold forever", harness.repo()), Some("ws20-1"))
        .await
        .expect("the daemon accepts a run");
    await_event(&harness, "task.attempt_started").await;

    let started = std::time::Instant::now();
    daemon.signal("TERM");
    // `shutdown_grace_period` is 3 s in the harness config; the drain, the
    // kill grace and the flush have to fit in a small multiple of it.
    let code = daemon.wait(Duration::from_secs(45)).await;
    let elapsed = started.elapsed();

    assert_eq!(code, Some(0), "a drained shutdown exits 0");
    assert!(
        elapsed < Duration::from_secs(30),
        "SIGTERM took {elapsed:?}; the grace period is 3s"
    );

    let failures = attempt_failures(&harness).await;
    assert!(
        failures.iter().any(|payload| {
            payload["message"].as_str() == Some("runtime_shutdown")
                && payload["class"].as_str() == Some("transient")
        }),
        "the running attempt must be terminalised as a transient \
         `runtime_shutdown` failure, got {failures:?}"
    );
    harness.close().await;
}

// ---------------------------------------------------------------------------
// 2 — health semantics
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws20_2_readyz_is_false_while_draining_or_db_down_healthz_stays_true() {
    kevin_testkit::skip_unless_pg!();
    let mut harness = Harness::with_scenario_and_extra(SCENARIO, AUTO_APPROVE).await;
    let daemon = harness.serve(&[]).await;
    let client = daemon.client(harness.token());

    let (status, body) = daemon.get("/healthz").await;
    assert_eq!(status, 200, "liveness: {body}");
    let ready = client.ready().await.expect("readyz answers");
    assert!(ready.ready && ready.db && !ready.draining, "{ready:?}");

    // -- draining ------------------------------------------------------------
    let drain = client.drain(true).await.expect("drain on");
    assert!(drain.draining, "POST /maintenance/drain closes admission");
    let (status, _) = daemon.get("/readyz").await;
    assert_eq!(status, 503, "a draining instance is not ready");
    let ready = client.ready().await.expect("readyz answers");
    assert!(ready.draining && !ready.ready && ready.db);
    let (status, _) = daemon.get("/healthz").await;
    assert_eq!(status, 200, "draining is not a liveness failure");

    // New runs are refused while draining (plan/10 §Health and drain).
    let refused = client
        .create_run(goal("while draining", harness.repo()), Some("ws20-2-drain"))
        .await
        .expect_err("a draining instance admits nothing");
    assert_eq!(refused.code(), Some("draining"), "{refused:?}");

    // -- undrain -------------------------------------------------------------
    let drain = client.drain(false).await.expect("drain off");
    assert!(!drain.draining);
    let (status, _) = daemon.get("/readyz").await;
    assert_eq!(status, 200, "DELETE /maintenance/drain re-opens admission");

    // -- database down -------------------------------------------------------
    harness.kill_database().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut db_down = false;
    while tokio::time::Instant::now() < deadline {
        if !client.ready().await.expect("readyz answers").db {
            db_down = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        db_down,
        "/readyz must report `db: false` once the db is gone"
    );
    let (status, _) = daemon.get("/readyz").await;
    assert_eq!(status, 503, "no database, no readiness");
    let (status, body) = daemon.get("/healthz").await;
    assert_eq!(
        status, 200,
        "/healthz never touches the database (plan/10): {body}"
    );

    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

// ---------------------------------------------------------------------------
// 3 — restart terminalises stale attempts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws20_3_restart_terminalises_stale_attempts_as_runtime_restarted() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario_and_extra(SCENARIO_HOLDING, AUTO_APPROVE).await;
    let daemon = harness.serve(&[]).await;
    daemon
        .client(harness.token())
        .create_run(goal("hold forever", harness.repo()), Some("ws20-3"))
        .await
        .expect("the daemon accepts a run");
    await_event(&harness, "task.attempt_started").await;

    // SIGKILL: the process never gets to record anything, exactly like a crash.
    daemon.signal("KILL");
    daemon.wait(Duration::from_secs(30)).await;
    assert!(
        attempt_failures(&harness).await.is_empty(),
        "a killed process records nothing"
    );

    // The next startup runs step 5 of plan/10 before it accepts work.
    let restarted = harness.serve(&[]).await;
    await_event(&harness, "task.attempt_failed").await;
    let failures = attempt_failures(&harness).await;
    assert!(
        failures.iter().any(|payload| {
            payload["class"].as_str() == Some("runtime_restarted")
                && payload["message"].as_str() == Some("runtime_restarted")
        }),
        "the stale attempt must come back as `runtime_restarted`, got {failures:?}"
    );

    restarted.signal("TERM");
    restarted.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

// ---------------------------------------------------------------------------
// 4 — metrics
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws20_4_metrics_endpoint_exposes_the_documented_names() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario_and_extra(SCENARIO, AUTO_APPROVE_WITH_METRICS).await;
    let daemon = harness.serve(&[]).await;
    let metrics_url = daemon
        .metrics_url()
        .expect("telemetry.metrics_bind starts a listener");
    assert!(
        !metrics_url.starts_with(daemon.api()),
        "/metrics is a separate listener, never the API bind (plan/10 §Metrics): \
         {metrics_url} vs {}",
        daemon.api()
    );

    // The API must not serve it.
    let (status, body) = daemon.get("/metrics").await;
    assert_ne!(status, 200, "the API bind never exposes /metrics");
    assert!(!body.contains("kevin_build_info"), "{body}");

    // Drive one complete run so the run/task/attempt families have samples.
    daemon
        .client(harness.token())
        .create_run(
            goal("add a healthz endpoint", harness.repo()),
            Some("ws20-4"),
        )
        .await
        .expect("the daemon accepts a run");
    await_event(&harness, "run.completed").await;
    // The gauge sweep runs every 5 s; give it one full pass.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let body = reqwest::get(metrics_url)
        .await
        .expect("scrape")
        .text()
        .await
        .expect("body");
    assert!(
        body.contains("# TYPE kevin_build_info"),
        "the exposition is Prometheus text: {}",
        &body[..body.len().min(200)]
    );

    // Everything a completed run and one gauge sweep must have produced.
    // The `worker_*` process metrics are deliberately absent: the `fake`
    // worker runs in-process and spawns nothing to count.
    let missing: Vec<&str> = [
        "kevin_build_info",
        "kevin_runs_total",
        "kevin_runs_active",
        "kevin_run_duration_seconds",
        "kevin_run_phase_duration_seconds",
        "kevin_tasks_total",
        "kevin_tasks_active",
        "kevin_task_attempts_total",
        "kevin_task_attempt_duration_seconds",
        "kevin_tokens_total",
        "kevin_cost_usd_total",
        "kevin_scheduler_ready_tasks",
        "kevin_scheduler_blocked_tasks",
        "kevin_event_store_append_duration_seconds",
        "kevin_events_appended_total",
        "kevin_outbox_backlog",
        "kevin_outbox_oldest_age_seconds",
        "kevin_projection_lag_events",
        "kevin_projection_apply_duration_seconds",
        "kevin_router_selections_total",
        "kevin_api_requests_total",
        "kevin_api_request_duration_seconds",
        "kevin_db_pool_connections",
        "kevin_kohral_draining",
    ]
    .into_iter()
    .filter(|name| !body.contains(name))
    .collect();
    assert!(
        missing.is_empty(),
        "the metrics endpoint must expose these after a completed run: {missing:?}"
    );
    // Labels are bounded enums, never ids (plan/10 §Metrics).
    assert!(
        body.contains(r"kevin_api_requests_total{") && !body.contains("run_id="),
        "no id ever becomes a label"
    );

    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

/// Every metric `plan/10` documents is emitted by production code, not only
/// declared in `kevin-telemetry`. A declared-but-never-recorded metric is
/// invisible to Prometheus, so the table in plan/10 would be a lie.
#[test]
fn ac_ws20_4_every_documented_metric_is_emitted_somewhere() {
    /// Every documented metric now has a call site; the list stays so a new
    /// declaration can be parked deliberately rather than by accident.
    const PENDING: &[&str] = &[];

    let sources = production_sources(&repo_root().join("crates"));
    assert!(sources.len() > 50, "found only {} sources", sources.len());
    let corpus: String = sources
        .iter()
        .filter(|path| !path.ends_with("kevin-telemetry/src/metrics.rs"))
        .map(|path| std::fs::read_to_string(path).unwrap_or_default())
        .collect();

    let declarations =
        std::fs::read_to_string(repo_root().join("crates/kevin-telemetry/src/metrics.rs"))
            .expect("the metric declarations");

    let mut missing = Vec::new();
    for line in declarations.lines() {
        let Some((konst, name)) = metric_declaration(line) else {
            continue;
        };
        if PENDING.contains(&name.as_str()) {
            continue;
        }
        // A call site names the metric either by its constant or by the
        // literal string (the telemetry crate itself does the latter).
        if !corpus.contains(&konst) && !corpus.contains(&format!("\"{name}\"")) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "declared in plan/10 but never recorded: {missing:?}"
    );
}

/// `    NAME = "kevin_x";` → `("NAME", "kevin_x")`.
fn metric_declaration(line: &str) -> Option<(String, String)> {
    let (konst, rest) = line.trim().split_once(" = \"")?;
    if !konst.chars().all(|c| c.is_ascii_uppercase() || c == '_') || konst.is_empty() {
        return None;
    }
    let name = rest.strip_suffix("\";")?;
    name.starts_with("kevin_")
        .then(|| (konst.to_owned(), name.to_owned()))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

/// Every `crates/*/src/**/*.rs`; tests and fixtures are not production code.
fn production_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = entry.file_name();
                if name != "tests" && name != "target" {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// 5 — a client attaches with a token; rotation needs no downtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws20_5_a_client_attaches_with_a_token_and_rotation_needs_no_downtime() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario_and_extra(SCENARIO, AUTO_APPROVE).await;
    let daemon = harness.serve(&[]).await;

    // This is what `kevin tui --server <url> --token-file <path>` does: one
    // `KevinClient` and nothing else (plan/07 §TUI, "no direct store access").
    let client = daemon.client(harness.token());
    let runs = client
        .list_runs(&kevin_api::dto::ListRunsQuery::default())
        .await
        .expect("an authenticated client reads the run list");
    assert!(runs.items.is_empty(), "a fresh daemon has no runs");
    assert!(
        !client.workers().await.expect("workers doctor").is_empty(),
        "the attached client sees the worker registry"
    );

    // No token, no answer.
    let anonymous = daemon.get("/api/v1/runs").await;
    assert_eq!(anonymous.0, 401, "the API is closed without a token");
    let wrong = daemon
        .client("not-the-token")
        .list_runs(&kevin_api::dto::ListRunsQuery::default())
        .await
        .expect_err("a wrong token is refused");
    assert_eq!(wrong.code(), Some("unauthenticated"), "{wrong:?}");

    // -- rotate --------------------------------------------------------------
    let old = harness.token().to_owned();
    harness
        .kevin_raw(&["config", "rotate-token"])
        .assert()
        .success();
    let new = std::fs::read_to_string(harness.token_file())
        .expect("token file")
        .trim()
        .to_owned();
    assert_ne!(new, old, "rotate-token writes a new secret");

    // Before the reload the daemon still only knows the old one.
    daemon
        .client(&new)
        .list_runs(&kevin_api::dto::ListRunsQuery::default())
        .await
        .expect_err("the new token is not live until SIGHUP");

    daemon.signal("HUP");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut reloaded = false;
    while tokio::time::Instant::now() < deadline {
        if daemon
            .client(&new)
            .list_runs(&kevin_api::dto::ListRunsQuery::default())
            .await
            .is_ok()
        {
            reloaded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reloaded, "SIGHUP must make the rotated token valid");
    assert!(
        daemon
            .client(&old)
            .list_runs(&kevin_api::dto::ListRunsQuery::default())
            .await
            .is_ok(),
        "the previous token keeps working for server.token_grace — that is what \
         makes the rotation downtime-free"
    );

    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// `kevin serve --kohral` binds the Kohral runtime contract on `kohral.bind`
/// next to the operator API (`plan/08-kohral-runtime.md` §6): a second
/// listener, a second token, one runtime behind both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_kohral_serves_the_runtime_contract() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario(SCENARIO).await;
    let daemon = harness.serve(&["--kohral", "--bind", "127.0.0.1:0"]).await;
    let kohral = daemon
        .kohral_url()
        .expect("--kohral must announce its listener")
        .to_owned();
    let client = reqwest::Client::new();

    // `/health` is the unauthenticated probe Kohral polls.
    let health = client
        .get(format!("{kohral}/health"))
        .send()
        .await
        .expect("GET /health");
    assert_eq!(health.status(), 200);
    let body: serde_json::Value = health.json().await.expect("health body");
    assert_eq!(body["platform"], "kevin");
    assert_eq!(body["status"], "ok");

    // The contract Kohral gates compatibility on, behind the Kohral token.
    let capabilities = client
        .get(format!("{kohral}/v1/capabilities"))
        .bearer_auth(harness.kohral_token())
        .send()
        .await
        .expect("GET /v1/capabilities");
    assert_eq!(capabilities.status(), 200);
    let body: serde_json::Value = capabilities.json().await.expect("capabilities body");
    for flag in [
        "run_idempotency_persistent",
        "run_status_persistent",
        "run_partial_output",
        "session_resources",
        "runtime_wide_drain",
        "runtime_model_catalog_v1",
    ] {
        assert_eq!(body["features"][flag], true, "{flag}: {body}");
    }
    assert_eq!(
        body["features"]["run_restart_failure_code"],
        "runtime_restarted"
    );
    assert_eq!(body["features"]["run_automatic_replay"], false);

    // The model catalog is served too, and the operator API token is not
    // accepted here: the two surfaces never share credentials.
    let models = client
        .get(format!("{kohral}/v1/kohral/models"))
        .bearer_auth(harness.kohral_token())
        .send()
        .await
        .expect("GET /v1/kohral/models");
    assert_eq!(models.status(), 200);
    let body: serde_json::Value = models.json().await.expect("catalog body");
    assert_eq!(body["object"], "kohral.runtime_model_catalog");
    assert_eq!(body["version"], 1);

    let wrong = client
        .get(format!("{kohral}/v1/capabilities"))
        .bearer_auth(harness.token())
        .send()
        .await
        .expect("GET with the API token");
    assert!(
        matches!(wrong.status().as_u16(), 401 | 403),
        "{}",
        wrong.status()
    );

    // And the operator API is still the operator API.
    let (status, _) = daemon.get("/healthz").await;
    assert_eq!(status, 200);

    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

/// Without `--kohral` there is no second listener at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_without_kohral_binds_only_the_api() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario(SCENARIO).await;
    let daemon = harness.serve(&["--bind", "127.0.0.1:0"]).await;
    assert!(
        daemon.kohral_url().is_none(),
        "the Kohral contract is opt-in"
    );
    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(45)).await;
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_prune_uses_the_retention_section() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario_and_extra(
        SCENARIO,
        "[retention]\ntask_log_days = 1\ntranscript_days = 1\nartifact_days = 1\n",
    )
    .await;

    // Two files under `data_dir`, one older than the horizon.
    let runs = harness.data_dir().join("runs/0191/0192");
    let artifacts = harness.data_dir().join("artifacts/0191");
    std::fs::create_dir_all(&runs).expect("mkdir");
    std::fs::create_dir_all(&artifacts).expect("mkdir");
    let stale = runs.join("old.jsonl");
    let fresh = runs.join("new.jsonl");
    let artifact = artifacts.join("integration.diff");
    for path in [&stale, &fresh, &artifact] {
        std::fs::write(path, "{}\n").expect("write");
    }
    backdate(&[&stale, &artifact], 3);

    let report = harness.json(&["--json", "db", "prune"]);
    assert_eq!(report["retention"]["task_log_days"], 1);
    assert_eq!(report["retention"]["transcript_days"], 1);
    assert_eq!(report["retention"]["artifact_days"], 1);
    assert_eq!(report["transcript_files"], 1, "only the stale transcript");
    assert_eq!(report["artifact_files"], 1);

    assert!(!stale.exists(), "the stale transcript is gone");
    assert!(fresh.exists(), "a fresh transcript is kept");
    assert!(!artifact.exists(), "the stale artifact copy is gone");
    harness.close().await;
}

/// Moves `paths` `days` into the past. `touch -t` is in POSIX and needs no
/// extra dependency for a two-line test helper.
fn backdate(paths: &[&Path], days: i64) {
    let stamp = (chrono::Local::now() - chrono::Duration::days(days))
        .format("%Y%m%d%H%M")
        .to_string();
    for path in paths {
        let status = std::process::Command::new("touch")
            .arg("-t")
            .arg(&stamp)
            .arg(path)
            .status()
            .expect("touch");
        assert!(status.success(), "could not backdate {}", path.display());
    }
}

#[test]
fn the_systemd_unit_is_hardened_and_reloadable() {
    let unit = std::fs::read_to_string(repo_root().join("deploy/systemd/kevin.service"))
        .expect("deploy/systemd/kevin.service");
    for directive in [
        "User=kevin",
        "EnvironmentFile=-/etc/kevin/kevin.env",
        "ExecStart=/usr/local/bin/kevin serve",
        "ExecReload=/bin/kill -HUP $MAINPID",
        "Restart=on-failure",
        "NoNewPrivileges=true",
        "ProtectSystem=strict",
        "ProtectHome=true",
        "PrivateTmp=true",
        "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
        "ReadWritePaths=/var/lib/kevin",
    ] {
        assert!(unit.contains(directive), "the unit must set `{directive}`");
    }
    // TimeoutStopSec must outlast shutdown_grace_period + kill_grace.
    assert!(unit.contains("TimeoutStopSec=60"));
    assert!(!unit.contains("User=root"), "the daemon never runs as root");

    let readme = std::fs::read_to_string(repo_root().join("deploy/systemd/README.md"))
        .expect("deploy/systemd/README.md");
    assert!(readme.contains("proxy_buffering    off"), "SSE needs it");
    assert!(readme.contains("rotate-token"), "token rotation procedure");

    let script = repo_root().join("deploy/scripts/backup-restore-test.sh");
    assert!(script.exists(), "the backup rehearsal script");
    let script = std::fs::read_to_string(&script).expect("script");
    assert!(script.contains("pg_dump --format=custom"));
    assert!(script.contains("rebuild-projection --all"));
}
