//! WS-14 acceptance tests (`plan/12-workstreams.md` §WS-13/14/15).
//!
//! Everything but the last test replays the golden fixtures under
//! `tests/fixtures/pi/` — either straight through [`PiStream`] or end to end
//! through the `fake-cli` shim standing in for the real binary. The real `pi`
//! CLI is only invoked by `ac_ws14_4_…`, which is `#[ignore]`d and additionally
//! requires `KEVIN_LIVE_TESTS=1`, so it never runs in CI.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_config::PiWorker as PiConfig;
use kevin_domain::{
    AttemptId, Effort, FailureClass, ModelAlias, RunId, TaskId, TaskKind, WorkerKind,
};
use kevin_worker::pi::{PiStream, PiWorker};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::worker::{AuthStatus, check_contract};
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, ModelEntry, Route, SandboxPolicy,
    TaskAttemptRequest, TaskSpec, Worker, WorkerError, WorkerEvent, WorkerOutcome, Workspace,
    WorkspacePolicy,
};
use rust_decimal::Decimal;
use serde_json::json;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-cli"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi")
        .join(name)
}

fn fixture_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture(name))
        .unwrap_or_else(|e| panic!("reading fixture {name}: {e}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Replays a fixture through the parser, returning every event it yields.
fn replay(name: &str) -> (PiStream, Vec<WorkerEvent>) {
    let mut stream = PiStream::new(Some(1234));
    let mut events = Vec::new();
    for line in fixture_lines(name) {
        events.extend(stream.parse_line(&line));
    }
    (stream, events)
}

/// A `PiWorker` whose binary is the `fake-cli` shim replaying `name`.
fn shim_worker(name: &str, data_dir: &Path, policy: SandboxPolicy) -> PiWorker {
    let cfg = PiConfig {
        bin: shim().to_string_lossy().into_owned(),
        // The shim ignores unknown flags, so the real pi argv is still built
        // and policy-checked; `--fixture <path>` makes it replay the golden.
        extra_args: vec![
            "--no-session".to_owned(),
            "--fixture".to_owned(),
            fixture(name).to_string_lossy().into_owned(),
        ],
        ..PiConfig::default()
    };
    PiWorker::new(cfg, policy, Duration::from_secs(5), data_dir)
}

fn entry() -> ModelEntry {
    ModelEntry::new(WorkerKind::Pi, "claude-sonnet-5").extra("provider", "anthropic")
}

fn request(workspace: &Path) -> TaskAttemptRequest {
    TaskAttemptRequest {
        attempt_id: AttemptId::new(),
        task_id: TaskId::new(),
        run_id: RunId::new(),
        kind: TaskKind::Implement,
        spec: TaskSpec::new(
            "Read the file",
            "Read hello.txt in the current directory and reply with only the JSON object.",
        ),
        route: Route {
            worker: WorkerKind::Pi,
            model: ModelAlias::new("sonnet5-pi").unwrap(),
            effort: None,
        },
        model: entry(),
        workspace: Workspace::in_place(workspace),
        context: AttemptContext::default(),
        env: EnvAllowlist::new(["PATH", "HOME"]),
        budget: AttemptBudget::with_timeout(Duration::from_secs(20)),
        cancel: CancellationToken::new(),
    }
}

fn kinds(events: &[WorkerEvent]) -> Vec<&'static str> {
    events.iter().map(WorkerEvent::kind_name).collect()
}

fn count(events: &[WorkerEvent], kind: &str) -> usize {
    events.iter().filter(|e| e.kind_name() == kind).count()
}

// ---------------------------------------------------------------------------
// (1) golden fixtures → expected event sequence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws14_1_golden_fixtures_map_to_the_expected_event_sequence() {
    // --- the real capture: session → thinking → usage → tool call/result →
    //     text deltas → usage. `pi` has no terminal line: the driver adds it.
    let (stream, events) = replay("success.jsonl");
    let mut expected = vec![
        "started",  // session header
        "thinking", // thinking_delta ×2
        "thinking",
        "usage",       // message_end of the tool-using assistant message
        "tool_call",   // tool_execution_start
        "tool_result", // tool_execution_end
    ];
    expected.extend(std::iter::repeat_n("assistant_text", 14)); // text_delta ×14
    expected.push("usage"); // message_end of the final assistant message
    assert_eq!(
        kinds(&events),
        expected,
        "agent_start/turn_*/agent_end/agent_settled and toolcall_* deltas are transcript-only"
    );
    assert!(matches!(
        &events[0],
        WorkerEvent::Started { session_id: Some(id), pid: Some(1234) }
            if id.as_str() == "11111111-2222-7333-8444-555555555555"
    ));
    assert!(matches!(
        &events[4],
        WorkerEvent::ToolCall { name, input_summary }
            if name == "read" && input_summary.contains("hello.txt")
    ));
    assert!(matches!(
        &events[5],
        WorkerEvent::ToolResult { name, ok: true, output_summary }
            if name == "read" && output_summary.contains("kevin-was-here")
    ));
    assert!(stream.saw_final());
    assert_eq!(stream.malformed_lines(), 0);
    assert_eq!(
        stream.final_text(),
        r#"{"status":"ok","contents":"kevin-was-here"}"#
    );

    // --- the same stream driven end to end through the supervisor
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let (events, outcome) = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .collect()
        .await;
    assert_eq!(kinds(&events), [expected.as_slice(), &["final"]].concat());
    assert!(check_contract(&events).is_ok());
    let WorkerOutcome::Succeeded {
        text,
        session_id,
        transcript,
        ..
    } = &outcome
    else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(text, r#"{"status":"ok","contents":"kevin-was-here"}"#);
    assert_eq!(
        session_id.as_ref().unwrap().as_str(),
        "11111111-2222-7333-8444-555555555555"
    );
    assert!(
        transcript.bytes > 0,
        "the raw stream is kept as a transcript"
    );
    assert!(Path::new(transcript.uri.trim_start_matches("file://")).is_file());

    // --- a provider error: exit 0, `stopReason = error` (real capture)
    let (stream, events) = replay("auth_error.jsonl");
    assert_eq!(
        kinds(&events),
        vec!["started"],
        "the failed turn spent nothing"
    );
    assert!(!stream.saw_final());
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("auth_error.jsonl", dir.path(), SandboxPolicy::cli_native());
    let (events, outcome) = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .collect()
        .await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Failed { class, message, .. } = &outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert_eq!(*class, FailureClass::Permanent);
    assert!(message.contains("OAuth refresh failed"), "{message}");

    // --- a 429 with pi's own retry loop (real capture)
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("rate_limit.jsonl", dir.path(), SandboxPolicy::cli_native());
    let (events, outcome) = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .collect()
        .await;
    check_contract(&events).expect("stream contract");
    assert_eq!(outcome.failure_class(), Some(FailureClass::Transient));
    assert!(
        outcome_message(&outcome).contains("429"),
        "{}",
        outcome_message(&outcome)
    );

    // --- `stopReason = length` (hand-written; see inferred.meta.toml)
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("truncated.jsonl", dir.path(), SandboxPolicy::cli_native());
    let outcome = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .wait()
        .await;
    assert_eq!(outcome.failure_class(), Some(FailureClass::Permanent));
    assert!(outcome_message(&outcome).contains("length"));
}

fn outcome_message(outcome: &WorkerOutcome) -> String {
    match outcome {
        WorkerOutcome::Failed { message, .. } => message.clone(),
        WorkerOutcome::Succeeded { text, .. } => text.clone(),
    }
}

// ---------------------------------------------------------------------------
// (2) usage extraction — pi reports both tokens and cost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws14_2_usage_and_cost_are_extracted_from_message_end() {
    let (stream, events) = replay("success.jsonl");
    // One `Usage` delta per assistant message, never per content delta:
    // `message_update.usage` stayed zero for the whole capture and
    // `message_end.message.usage` is the total of that one message.
    assert_eq!(count(&events, "usage"), 2);
    let deltas: Vec<&kevin_worker::Usage> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Usage { delta } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas[0].input_tokens, 1196);
    assert_eq!(deltas[0].output_tokens, 32);
    assert_eq!(deltas[1].cache_read_tokens, 1024);

    // The stream total is the sum of the per-message totals.
    let total = stream.usage();
    assert_eq!(total.input_tokens, 1196 + 221);
    assert_eq!(total.output_tokens, 32 + 18);
    assert_eq!(total.cache_read_tokens, 1024);
    assert_eq!(total.cache_write_tokens, 0);
    // `pi` reports cost itself (`usage.cost.total`), so the router price table
    // is never consulted for a pi attempt.
    assert_eq!(total.cost_usd, Some(Decimal::new(136_455, 8)));

    // End to end the outcome carries the same usage, plus the wall clock.
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let outcome = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .wait()
        .await;
    assert_eq!(outcome.usage().total_tokens(), 1196 + 32 + 221 + 18);
    assert_eq!(outcome.usage().cost_usd, Some(Decimal::new(136_455, 8)));

    // A turn that failed before reaching a provider reports no usage at all.
    let (stream, _) = replay("auth_error.jsonl");
    assert!(stream.usage().is_empty());
}

// ---------------------------------------------------------------------------
// (3) policy + validate_alias
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws14_3_policy_and_alias_validation_are_enforced() {
    let dir = tempfile::tempdir().unwrap();

    // (a) `pi` has no bypass flag of its own, but `extra_args` must not smuggle
    //     one in outside the container tier.
    let smuggled = PiConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--dangerously-skip-permissions".to_owned()],
        ..PiConfig::default()
    };
    let native = PiWorker::new(
        smuggled.clone(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let err = native.start(request(dir.path())).await.unwrap_err();
    assert!(
        matches!(&err, WorkerError::PolicyViolation { flag, tier }
            if flag == "--dangerously-skip-permissions" && tier == "cli-native"),
        "{err}"
    );
    let container = PiWorker::new(
        smuggled,
        SandboxPolicy::container(),
        Duration::from_secs(1),
        dir.path(),
    );
    assert!(container.build_argv(&request(dir.path()), None).is_ok());

    // (b) a read-only, in-place attempt is limited to pi's read-only tools.
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut read_only = request(dir.path());
    read_only.spec.workspace_policy = WorkspacePolicy::ReadOnly;
    let argv = worker.build_argv(&read_only, None).unwrap().join(" ");
    assert!(argv.contains("--tools read,grep,find,ls"), "{argv}");

    // (c) `validate_alias`: a pi alias without `provider` is rejected …
    let alias = ModelAlias::new("sonnet5-pi").unwrap();
    let err = worker
        .validate_alias(&alias, &ModelEntry::new(WorkerKind::Pi, "claude-sonnet-5"))
        .unwrap_err();
    assert!(err.to_string().contains("provider"), "{err}");
    // … and so is one whose `provider` is empty, or that names another worker.
    assert!(
        worker
            .validate_alias(
                &alias,
                &ModelEntry::new(WorkerKind::Pi, "claude-sonnet-5").extra("provider", "  ")
            )
            .is_err()
    );
    assert!(
        worker
            .validate_alias(
                &alias,
                &ModelEntry::new(WorkerKind::Claude, "claude-sonnet-5")
            )
            .is_err()
    );
    worker.validate_alias(&alias, &entry()).unwrap();

    // (d) without a provider nothing can be spawned: `start` fails fast.
    let mut no_provider = request(dir.path());
    no_provider.model = ModelEntry::new(WorkerKind::Pi, "claude-sonnet-5");
    assert!(matches!(
        worker.start(no_provider).await,
        Err(WorkerError::InvalidAlias { .. })
    ));

    // (e) the registry builds pi from the default config and validates every
    //     `[models.*]` alias it serves.
    let cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    };
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let pi = registry.get(WorkerKind::Pi).expect("pi registered");
    assert_eq!(pi.kind(), WorkerKind::Pi);
    let aliases = cfg.aliases_for(WorkerKind::Pi);
    assert!(!aliases.is_empty(), "the default catalog has a pi alias");
    for alias in aliases {
        pi.validate_alias(&alias, &cfg.models[&alias]).unwrap();
    }
}

// ---------------------------------------------------------------------------
// (4) doctor: binary + auth; live smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws14_4_doctor_detects_the_binary_and_auth() {
    let dir = tempfile::tempdir().unwrap();

    // A missing binary is reported, never a panic, and auth is not probed.
    let missing = PiWorker::new(
        PiConfig {
            bin: "definitely-not-pi-kevin".to_owned(),
            ..PiConfig::default()
        },
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let doctor = missing.doctor().await;
    assert_eq!(doctor.kind, WorkerKind::Pi);
    assert!(doctor.binary.is_none());
    assert!(!doctor.is_healthy());
    assert!(doctor.notes[0].contains("workers.pi.bin"), "{doctor:?}");

    // The shim stands in for `pi`: the binary and `--version` are found, and
    // `pi auth check` (which the shim does not implement) leaves auth unknown
    // rather than claiming readiness.
    let present = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native())
        .with_providers(["anthropic"]);
    assert_eq!(present.providers(), ["anthropic"]);
    let doctor = present.doctor().await;
    assert!(doctor.binary.is_some());
    assert!(
        doctor
            .version
            .as_deref()
            .is_some_and(|v| v.starts_with("fake-cli")),
        "{doctor:?}"
    );
    assert_eq!(doctor.auth_ready, AuthStatus::Unknown);
    assert!(doctor.is_healthy(), "unknown auth is not a failure");

    // `kevin workers doctor` gets a real row for pi (not the generic probe).
    let cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    }
    .with_bin(WorkerKind::Pi, shim().to_string_lossy().into_owned());
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let doctors = registry.doctor_all().await;
    let row = doctors
        .iter()
        .find(|d| d.kind == WorkerKind::Pi)
        .expect("doctor row for pi");
    assert!(
        !row.notes
            .iter()
            .any(|n| n.contains("adapter not available")),
        "{row:?}"
    );
    assert!(row.binary.is_some());
}

/// Runs the *real* `pi` CLI once with a trivial prompt.
///
/// Doubly gated: `#[ignore]` (nextest/cargo skip it by default) *and* an
/// explicit `KEVIN_LIVE_TESTS=1`. Run it with
/// `KEVIN_LIVE_TESTS=1 cargo nextest run -p kevin-worker --run-ignored all
/// ac_ws14_5`. It uses the cheapest available alias, one message, no tools.
#[tokio::test]
#[ignore = "live: spends money; set KEVIN_LIVE_TESTS=1 and pass --run-ignored"]
async fn ac_ws14_5_live_smoke_runs_pi_once() {
    if std::env::var("KEVIN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: KEVIN_LIVE_TESTS != 1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = PiConfig {
        extra_args: vec![
            "--no-session".to_owned(),
            "--no-context-files".to_owned(),
            "--no-tools".to_owned(),
        ],
        ..PiConfig::default()
    };
    let worker = PiWorker::new(
        cfg,
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        dir.path(),
    )
    .with_providers([live_provider()]);
    let doctor = worker.doctor().await;
    assert!(
        doctor.binary.is_some(),
        "the real `pi` binary must be on PATH for the live test"
    );
    assert_ne!(
        doctor.auth_ready,
        AuthStatus::Missing(String::new()),
        "pi auth check must not report a missing provider: {doctor:?}"
    );

    let mut req = request(dir.path());
    req.model = ModelEntry::new(WorkerKind::Pi, live_model()).extra("provider", live_provider());
    req.route.effort = Some(Effort::Low);
    req.spec = TaskSpec::new(
        "live smoke",
        "Reply with exactly the word: kevin. Do not use any tool.",
    );
    req.budget = AttemptBudget::with_timeout(Duration::from_secs(180));

    let (events, outcome) = worker.start(req).await.expect("spawn").collect().await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Succeeded { text, usage, .. } = &outcome else {
        panic!("live pi run failed: {outcome:?}");
    };
    assert!(
        text.to_lowercase().contains("kevin"),
        "unexpected answer: {text}"
    );
    assert!(usage.output_tokens > 0);
    assert!(usage.cost_usd.is_some(), "pi reports usage.cost.total");
    assert!(
        usage.cost_usd.unwrap() < Decimal::new(10, 2),
        "live smoke must stay under $0.10, got {:?}",
        usage.cost_usd
    );
}

fn live_provider() -> String {
    std::env::var("KEVIN_LIVE_PI_PROVIDER").unwrap_or_else(|_| "anthropic".to_owned())
}

fn live_model() -> String {
    std::env::var("KEVIN_LIVE_PI_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_owned())
}

// ---------------------------------------------------------------------------
// Supporting tests
// ---------------------------------------------------------------------------

/// `plan/11-testing.md` §Worker adapter testing (1): argv per tier ×
/// {schema, none} × {fresh, resume}.
#[test]
fn argv_snapshot_per_tier_schema_and_session() {
    let worker = PiWorker::new(
        PiConfig::default(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        "/data",
    );
    let mut req = request(Path::new("/workspace"));
    req.attempt_id = AttemptId::nil();
    let fresh = worker.build_argv(&req, None).unwrap();
    assert_eq!(
        fresh.join(" "),
        format!(
            "-p --mode json --provider anthropic --model claude-sonnet-5 \
             --append-system-prompt {} --no-session {}",
            kevin_worker::pi::briefing(&req),
            req.spec.instructions
        )
    );

    // With a schema the instruction goes into the appended system prompt —
    // `pi` has no schema flag (`plan/04` §Structured output).
    req.spec.output_schema = Some(json!({"type": "object", "required": ["status"]}));
    req.route.effort = Some(Effort::High);
    let with_schema = worker.build_argv(&req, None).unwrap();
    let i = with_schema
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap();
    assert!(with_schema[i + 1].contains(
        r#"Respond with only a JSON object matching this schema: {"required":["status"],"type":"object"}"#
    ));
    assert!(with_schema.windows(2).any(|w| w == ["--thinking", "high"]));
    assert!(!with_schema.iter().any(|a| a == "--json-schema"));

    // Resuming names the session and drops the contradicting `--no-session`.
    let resumed = worker.build_argv(&req, Some("sess-42")).unwrap();
    assert!(resumed.windows(2).any(|w| w == ["--session", "sess-42"]));
    assert!(!resumed.iter().any(|a| a == "--no-session"));

    // Sessions on: a fresh attempt names itself so a follow-up can resume it.
    let sessioned = PiWorker::new(
        PiConfig {
            extra_args: Vec::new(),
            ..PiConfig::default()
        },
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        "/data",
    );
    let argv = sessioned.build_argv(&req, None).unwrap();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--session-id", AttemptId::nil().to_string().as_str()])
    );
}

/// `plan/04` §Structured output (2)+(3) for a CLI with no schema flag.
#[tokio::test]
async fn structured_output_is_extracted_from_the_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("structured.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    req.spec.output_schema = Some(json!({
        "type": "object",
        "properties": {"status": {"type": "string"}, "files_changed": {"type": "integer"}},
        "required": ["status", "files_changed"]
    }));
    let (events, outcome) = worker.start(req).await.expect("spawn").collect().await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Succeeded { structured, .. } = &outcome else {
        panic!("expected success, got {outcome:?}");
    };
    // Prose around a ```json fence containing a trailing comma.
    assert_eq!(structured.as_ref().unwrap()["status"], "ok");
    assert_eq!(structured.as_ref().unwrap()["files_changed"], 2);
}

#[tokio::test]
async fn a_schema_violation_that_cannot_be_repaired_is_permanent() {
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("structured.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    // The fixture answers {"status":"ok","files_changed":2}; demand something else.
    req.spec.output_schema = Some(json!({
        "type": "object",
        "properties": {"verdict": {"type": "string"}},
        "required": ["verdict"]
    }));
    let (events, outcome) = worker.start(req).await.expect("spawn").collect().await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Failed { class, message, .. } = &outcome else {
        panic!("expected failure, got {outcome:?}");
    };
    assert_eq!(*class, FailureClass::Permanent);
    assert!(message.starts_with("schema_violation:"), "{message}");
    // The repair turn ran and its usage was added to the first turn's.
    assert_eq!(outcome.usage().input_tokens, 2 * 1204);
}

#[tokio::test]
async fn cancellation_and_a_missing_final_are_classified() {
    let dir = tempfile::tempdir().unwrap();

    // A child that emits nothing exits 0 without a final assistant message.
    let worker = PiWorker::new(
        PiConfig {
            bin: shim().to_string_lossy().into_owned(),
            extra_args: vec!["--stderr".to_owned(), "nothing to do".to_owned()],
            ..PiConfig::default()
        },
        SandboxPolicy::cli_native(),
        Duration::from_millis(200),
        dir.path(),
    );
    let (events, outcome) = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .collect()
        .await;
    check_contract(&events).expect("stream contract");
    assert_eq!(outcome.failure_class(), Some(FailureClass::Permanent));

    // Cancellation mid-stream.
    let worker = PiWorker::new(
        PiConfig {
            bin: shim().to_string_lossy().into_owned(),
            extra_args: vec!["--hang".to_owned()],
            ..PiConfig::default()
        },
        SandboxPolicy::cli_native(),
        Duration::from_millis(200),
        dir.path(),
    );
    let handle = worker.start(request(dir.path())).await.expect("spawn");
    handle.cancel();
    let outcome = handle.wait().await;
    assert_eq!(outcome.failure_class(), Some(FailureClass::Cancelled));
}

#[test]
fn the_parser_never_panics_on_arbitrary_lines() {
    let mut stream = PiStream::new(None);
    for line in [
        "",
        "   ",
        "null",
        "[]",
        "42",
        "{}",
        r#"{"type":"session"}"#,
        r#"{"type":"message_update"}"#,
        r#"{"type":"message_update","assistantMessageEvent":{}}"#,
        r#"{"type":"message_end"}"#,
        r#"{"type":"message_end","message":{}}"#,
        r#"{"type":"message_end","message":{"role":"assistant"}}"#,
        r#"{"type":"tool_execution_start"}"#,
        r#"{"type":"tool_execution_end","result":null}"#,
        r#"{"type":"agent_end","messages":[],"willRetry":false}"#,
        "{\"type\":\"session\",\"id\":\"\u{1F600}\"}",
    ] {
        let _ = stream.parse_line(line);
    }
    assert!(stream.malformed_lines() >= 3);
}
