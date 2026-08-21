//! WS-06 acceptance tests (`plan/12-workstreams.md` §WS-06).
//!
//! Everything but the last test replays the golden fixtures under
//! `tests/fixtures/claude/` — either straight through [`ClaudeStream`] or
//! end-to-end through the `fake-cli` shim standing in for the real binary. The
//! real `claude` CLI is only invoked by `ac_ws06_4_…`, which is `#[ignore]`d
//! and additionally requires `KEVIN_LIVE_TESTS=1`, so it never runs in CI.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_config::{ClaudePermissionMode, ClaudeWorker as ClaudeConfig, StructuredOutput};
use kevin_domain::{
    AttemptId, Effort, FailureClass, ModelAlias, RunId, TaskId, TaskKind, WorkerKind,
};
use kevin_worker::claude::{ClaudeStream, ClaudeWorker};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::worker::check_contract;
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, ModelEntry, Route, SandboxPolicy,
    TaskAttemptRequest, TaskSpec, Worker, WorkerError, WorkerEvent, WorkerOutcome, Workspace,
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
        .join("tests/fixtures/claude")
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
fn replay(name: &str) -> (ClaudeStream, Vec<WorkerEvent>) {
    let mut stream = ClaudeStream::new(Some(1234));
    let mut events = Vec::new();
    for line in fixture_lines(name) {
        events.extend(stream.parse_line(&line));
    }
    (stream, events)
}

/// A `ClaudeWorker` whose binary is the `fake-cli` shim replaying `name`.
fn shim_worker(name: &str, data_dir: &Path, policy: SandboxPolicy) -> ClaudeWorker {
    let mut cfg = ClaudeConfig {
        bin: shim().to_string_lossy().into_owned(),
        // The shim ignores unknown flags, so the real claude argv is still
        // built and checked; `--fixture <path>` makes it replay the golden.
        extra_args: vec![
            "--fixture".to_owned(),
            fixture(name).to_string_lossy().into_owned(),
        ],
        ..ClaudeConfig::default()
    };
    cfg.max_turns = 4;
    ClaudeWorker::new(cfg, policy, Duration::from_secs(5), data_dir)
}

fn request(workspace: &Path) -> TaskAttemptRequest {
    TaskAttemptRequest {
        attempt_id: AttemptId::new(),
        task_id: TaskId::new(),
        run_id: RunId::new(),
        kind: TaskKind::Implement,
        spec: TaskSpec::new(
            "Print the string",
            "Read main.rs and reply with what it prints.",
        ),
        route: Route {
            worker: WorkerKind::Claude,
            model: ModelAlias::new("haiku45-claude").unwrap(),
            effort: None,
        },
        model: ModelEntry::new(WorkerKind::Claude, "claude-haiku-4-5"),
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

// ---------------------------------------------------------------------------
// (1) golden fixtures → expected event sequence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws06_1_golden_fixtures_map_to_the_expected_event_sequence() {
    // --- the real capture: init → thinking → tool_use → tool_result → text → result
    let (stream, events) = replay("success.jsonl");
    assert_eq!(
        kinds(&events),
        vec![
            "started",     // system/init
            "thinking",    // assistant thinking block
            "usage",       //   its message.usage
            "tool_call",   // assistant tool_use Read
            "tool_result", // user tool_result
            "thinking",    // second assistant message
            "usage",
            "assistant_text",
            "final", // result/success
        ],
        "hook lifecycle, thinking_tokens and rate_limit_event lines are transcript-only"
    );
    assert!(matches!(
        &events[0],
        WorkerEvent::Started { session_id: Some(id), pid: Some(1234) }
            if id.as_str() == "11111111-2222-4333-8444-555555555555"
    ));
    assert!(matches!(
        &events[3],
        WorkerEvent::ToolCall { name, input_summary }
            if name == "Read" && input_summary.contains("/workspace/main.rs")
    ));
    assert!(matches!(
        &events[4],
        WorkerEvent::ToolResult { name, ok: true, output_summary }
            if name == "Read" && output_summary.contains("fn main()")
    ));
    assert!(matches!(&events[7], WorkerEvent::AssistantText { delta } if delta == "kevin"));
    assert!(matches!(&events[8], WorkerEvent::Final { text, .. } if text == "kevin"));
    assert!(stream.saw_final());
    assert_eq!(stream.malformed_lines(), 0);
    assert!(check_contract(&events).is_ok());

    // --- the same stream driven end to end through the supervisor
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let (events, outcome) = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .collect()
        .await;
    assert_eq!(kinds(&events), kinds(&replay("success.jsonl").1));
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
    assert_eq!(text, "kevin");
    assert_eq!(
        session_id.as_ref().unwrap().as_str(),
        "11111111-2222-4333-8444-555555555555"
    );
    assert!(
        transcript.bytes > 0,
        "the raw stream is kept as a transcript"
    );
    assert!(Path::new(transcript.uri.trim_start_matches("file://")).is_file());

    // --- error subtypes
    let (_, events) = replay("error_max_turns.jsonl");
    assert_eq!(
        kinds(&events),
        vec!["started", "tool_call", "usage", "tool_result", "failed"]
    );
    assert!(matches!(
        &events[3],
        WorkerEvent::ToolResult { ok: false, name, .. } if name == "Bash"
    ));
    assert!(matches!(
        &events[4],
        WorkerEvent::Failed { class: FailureClass::Permanent, message, .. }
            if message.contains("error_max_turns")
    ));

    let (_, events) = replay("rate_limit.jsonl");
    assert_eq!(kinds(&events), vec!["started", "failed"]);
    assert!(matches!(
        &events[1],
        WorkerEvent::Failed { class: FailureClass::Transient, message, .. }
            if message.contains("429")
    ));

    // --- structured output reported by the CLI
    let (stream, events) = replay("structured.jsonl");
    assert_eq!(
        kinds(&events),
        vec!["started", "assistant_text", "usage", "final"]
    );
    assert_eq!(stream.structured().unwrap()["files_changed"], 2);
}

// ---------------------------------------------------------------------------
// (2) usage + total_cost_usd extraction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws06_2_usage_and_total_cost_usd_are_extracted() {
    let (stream, events) = replay("success.jsonl");
    let WorkerEvent::Final { usage, .. } = events.last().unwrap() else {
        panic!("expected Final");
    };
    // Exactly the `result.usage` numbers of the captured stream.
    assert_eq!(usage.input_tokens, 18);
    assert_eq!(usage.output_tokens, 222);
    assert_eq!(usage.cache_write_tokens, 8360);
    assert_eq!(usage.cache_read_tokens, 46878);
    assert_eq!(usage.wall_ms, 4160);
    // `total_cost_usd` from the result line — claude is the one worker that
    // reports cost, so the router price table is never consulted.
    assert_eq!(usage.cost_usd, Some(Decimal::new(2_253_580, 8)));
    assert_eq!(stream.usage(), usage);

    // Incremental `message.usage` events sum to the message totals and are
    // never double counted when the CLI repeats them per content block.
    let deltas: Vec<&kevin_worker::Usage> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Usage { delta } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas.len(),
        2,
        "one per assistant message, not per content block"
    );
    assert!(deltas.iter().all(|d| d.cost_usd.is_none()));

    // End to end the outcome carries the same usage.
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let outcome = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .wait()
        .await;
    assert_eq!(outcome.usage().cost_usd, Some(Decimal::new(2_253_580, 8)));
    assert_eq!(outcome.usage().total_tokens(), 240);
}

// ---------------------------------------------------------------------------
// (3) bypass flags rejected under cli-native
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws06_3_bypass_flag_is_rejected_under_cli_native() {
    let dir = tempfile::tempdir().unwrap();

    // (a) permission_mode = bypassPermissions
    let mut bypass = ClaudeConfig {
        bin: shim().to_string_lossy().into_owned(),
        ..ClaudeConfig::default()
    };
    bypass.permission_mode = ClaudePermissionMode::BypassPermissions;
    let native = ClaudeWorker::new(
        bypass.clone(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let err = native.start(request(dir.path())).await.unwrap_err();
    assert!(
        matches!(&err, WorkerError::PolicyViolation { flag, tier }
            if flag == "bypassPermissions" && tier == "cli-native"),
        "{err}"
    );

    // (b) a dangerous flag smuggled through extra_args
    let mut smuggled = ClaudeConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--dangerously-skip-permissions".to_owned()],
        ..ClaudeConfig::default()
    };
    smuggled.max_turns = 1;
    let native = ClaudeWorker::new(
        smuggled.clone(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let err = native.start(request(dir.path())).await.unwrap_err();
    assert!(
        matches!(&err, WorkerError::PolicyViolation { flag, .. }
            if flag == "--dangerously-skip-permissions"),
        "{err}"
    );

    // (c) the container tier allows both.
    for cfg in [bypass, smuggled] {
        let container = ClaudeWorker::new(
            cfg,
            SandboxPolicy::container(),
            Duration::from_secs(1),
            dir.path(),
        );
        assert!(container.build_argv(&request(dir.path()), None).is_ok());
    }

    // (d) the registry refuses to build a bypassing config outside container.
    let mut cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    };
    cfg.claude.permission_mode = ClaudePermissionMode::BypassPermissions;
    let errs =
        WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).expect_err("must reject");
    assert!(
        errs.to_string().contains("workers.claude.permission_mode"),
        "{errs}"
    );
    assert!(WorkerRegistry::from_config(&cfg, SandboxPolicy::container()).is_ok());
}

// ---------------------------------------------------------------------------
// (4) live smoke test — opt-in only, never in CI
// ---------------------------------------------------------------------------

/// Runs the *real* `claude` CLI once with a trivial prompt.
///
/// Doubly gated: `#[ignore]` (nextest/cargo skip it by default) *and* an
/// explicit `KEVIN_LIVE_TESTS=1`. Run it with
/// `KEVIN_LIVE_TESTS=1 cargo nextest run -p kevin-worker --run-ignored all
/// ac_ws06_4`. The attempt is capped at `--max-turns 1` and, when the CLI
/// supports it, `--max-budget-usd 0.10` on the cheapest alias.
#[tokio::test]
#[ignore = "live: spends money; set KEVIN_LIVE_TESTS=1 and pass --run-ignored"]
async fn ac_ws06_4_live_smoke_runs_claude_once() {
    if std::env::var("KEVIN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: KEVIN_LIVE_TESTS != 1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = ClaudeConfig {
        extra_args: vec!["--max-budget-usd".to_owned(), "0.10".to_owned()],
        allowed_tools: Vec::new(),
        ..ClaudeConfig::default()
    };
    cfg.max_turns = 1;
    cfg.permission_mode = ClaudePermissionMode::Plan;
    cfg.structured_output = StructuredOutput::None;
    let worker = ClaudeWorker::new(
        cfg,
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        dir.path(),
    );
    assert!(
        worker.doctor().await.binary.is_some(),
        "the real `claude` binary must be on PATH for the live test"
    );

    let mut req = request(dir.path());
    req.spec = TaskSpec::new(
        "live smoke",
        "Reply with exactly the word: kevin. Do not use any tool.",
    );
    req.budget = AttemptBudget::with_timeout(Duration::from_secs(180));

    let (events, outcome) = worker.start(req).await.expect("spawn").collect().await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Succeeded { text, usage, .. } = &outcome else {
        panic!("live claude run failed: {outcome:?}");
    };
    assert!(
        text.to_lowercase().contains("kevin"),
        "unexpected answer: {text}"
    );
    assert!(usage.output_tokens > 0);
    assert!(usage.cost_usd.is_some(), "claude reports total_cost_usd");
    assert!(
        usage.cost_usd.unwrap() < Decimal::new(10, 2),
        "live smoke must stay under $0.10, got {:?}",
        usage.cost_usd
    );
}

// ---------------------------------------------------------------------------
// Supporting tests
// ---------------------------------------------------------------------------

/// `plan/11-testing.md` §Worker adapter testing (1): argv per tier ×
/// {schema, none} × {fresh, resume}.
#[test]
fn argv_snapshot_per_tier_schema_and_session() {
    let worker = ClaudeWorker::new(
        ClaudeConfig::default(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        "/data",
    );
    let mut req = request(Path::new("/workspace"));
    req.attempt_id = AttemptId::nil();
    let fresh = worker.build_argv(&req, None).unwrap();
    let joined = fresh.join(" ");
    assert_eq!(
        joined,
        format!(
            "-p --output-format stream-json --verbose --model claude-haiku-4-5 \
             --permission-mode acceptEdits \
             --allowedTools Read Edit Write Bash(git *) Bash(cargo *) Bash(npm *) Bash(pnpm *) Bash(bun *) Grep Glob \
             --append-system-prompt {} --session-id {} --max-turns 200",
            kevin_worker::claude::briefing(&req),
            AttemptId::nil()
        )
    );

    req.spec.output_schema = Some(json!({"type": "object", "required": ["status"]}));
    req.route.effort = Some(Effort::High);
    let with_schema = worker.build_argv(&req, None).unwrap();
    let i = with_schema
        .iter()
        .position(|a| a == "--json-schema")
        .unwrap();
    assert_eq!(
        with_schema[i + 1],
        r#"{"required":["status"],"type":"object"}"#
    );
    assert!(with_schema.windows(2).any(|w| w == ["--effort", "high"]));

    let resumed = worker.build_argv(&req, Some("sess-42")).unwrap();
    assert!(resumed.windows(2).any(|w| w == ["--resume", "sess-42"]));
    assert!(!resumed.iter().any(|a| a == "--session-id"));
}

#[tokio::test]
async fn structured_output_is_taken_from_the_cli_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("structured.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    req.spec.output_schema = Some(json!({
        "type": "object",
        "properties": {"status": {"type": "string"}, "files_changed": {"type": "integer"}},
        "required": ["status", "files_changed"]
    }));
    let outcome = worker.start(req).await.expect("spawn").wait().await;
    let WorkerOutcome::Succeeded { structured, .. } = &outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(structured.as_ref().unwrap()["status"], "ok");
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
}

/// Without a native `structured_output` the adapter falls back to
/// `structured::extract_and_validate` on the final text.
#[test]
fn structured_output_falls_back_to_extraction_from_the_final_text() {
    let mut stream = ClaudeStream::new(None);
    let _ = stream.parse_line(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let events = stream.parse_line(
        &json!({"type":"result","subtype":"success","is_error":false,
            "result":"Here you go:\n```json\n{\"status\": \"ok\",}\n```"})
        .to_string(),
    );
    assert!(matches!(
        &events[0],
        WorkerEvent::Final {
            structured: None,
            ..
        }
    ));
    let value = kevin_worker::structured::extract_and_validate(
        &stream.final_text(),
        &json!({"type": "object", "required": ["status"]}),
    )
    .expect("fenced JSON with a trailing comma is repaired");
    assert_eq!(value["status"], "ok");
}

#[tokio::test]
async fn registry_registers_claude_and_doctor_lists_it() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    };
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let claude = registry.get(WorkerKind::Claude).expect("claude registered");
    assert_eq!(claude.kind(), WorkerKind::Claude);

    // Every default `[models.*]` alias served by claude validates.
    for alias in cfg.aliases_for(WorkerKind::Claude) {
        claude.validate_alias(&alias, &cfg.models[&alias]).unwrap();
    }

    // `kevin workers doctor` gets a row for claude, whatever the binary state.
    let doctors = registry.doctor_all().await;
    let row = doctors
        .iter()
        .find(|d| d.kind == WorkerKind::Claude)
        .expect("doctor row for claude");
    assert!(
        !row.notes
            .iter()
            .any(|n| n.contains("adapter not available"))
    );

    // A missing binary is reported, never a panic.
    let cfg = cfg.with_bin(WorkerKind::Claude, "definitely-not-claude-kevin");
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let doctors = registry.doctor_all().await;
    let row = doctors
        .iter()
        .find(|d| d.kind == WorkerKind::Claude)
        .unwrap();
    assert!(row.binary.is_none());
    assert!(!row.is_healthy());
}

#[tokio::test]
async fn cancellation_and_a_missing_final_are_classified() {
    let dir = tempfile::tempdir().unwrap();

    // A child that never emits a `result` line exits 0 without a Final.
    let mut cfg = ClaudeConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--stderr".to_owned(), "nothing to do".to_owned()],
        ..ClaudeConfig::default()
    };
    cfg.max_turns = 1;
    let worker = ClaudeWorker::new(
        cfg,
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
    let mut cfg = ClaudeConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--hang".to_owned()],
        ..ClaudeConfig::default()
    };
    cfg.max_turns = 1;
    let worker = ClaudeWorker::new(
        cfg,
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
    let mut stream = ClaudeStream::new(None);
    for line in [
        "",
        "   ",
        "null",
        "[]",
        "\"a string\"",
        "{}",
        r#"{"type":"assistant"}"#,
        r#"{"type":"assistant","message":{}}"#,
        r#"{"type":"assistant","message":{"content":"not an array"}}"#,
        r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#,
        r#"{"type":"result"}"#,
        r#"{"type":"unknown"}"#,
        "{\"type\":\"system\",\"subtype\":\"init\"",
    ] {
        let _ = stream.parse_line(line);
    }
    assert!(stream.session_id().is_none());
}
