//! WS-13 acceptance tests (`plan/12-workstreams.md` §WS-13).
//!
//! Everything but the last test replays the golden fixtures under
//! `tests/fixtures/codex/` — either straight through [`CodexStream`] or
//! end-to-end through the `fake-cli` shim standing in for the real binary. The
//! real `codex` CLI is only invoked by `ac_ws13_5_…`, which is `#[ignore]`d and
//! additionally requires `KEVIN_LIVE_TESTS=1`, so it never runs in CI.

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_config::{CodexSandbox, CodexWorker as CodexConfig};
use kevin_domain::{
    AttemptId, Effort, FailureClass, ModelAlias, RunId, TaskId, TaskKind, WorkerKind,
};
use kevin_worker::codex::{CodexStream, CodexWorker};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::worker::{AuthStatus, check_contract};
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, ModelEntry, Route, SandboxPolicy,
    TaskAttemptRequest, TaskSpec, Usage, Worker, WorkerError, WorkerEvent, WorkerOutcome,
    Workspace,
};
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
        .join("tests/fixtures/codex")
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
fn replay(name: &str) -> (CodexStream, Vec<WorkerEvent>) {
    let mut stream = CodexStream::new(Some(1234));
    let mut events = Vec::new();
    for line in fixture_lines(name) {
        events.extend(stream.parse_line(&line));
    }
    (stream, events)
}

/// A `CodexWorker` whose binary is the `fake-cli` shim replaying `name`.
fn shim_worker(name: &str, data_dir: &Path, policy: SandboxPolicy) -> CodexWorker {
    let cfg = CodexConfig {
        bin: shim().to_string_lossy().into_owned(),
        // The shim ignores unknown flags, so the real codex argv is still
        // built and checked; `--fixture <path>` makes it replay the golden.
        extra_args: vec![
            "--fixture".to_owned(),
            fixture(name).to_string_lossy().into_owned(),
        ],
        ..CodexConfig::default()
    };
    CodexWorker::new(cfg, policy, Duration::from_secs(5), data_dir)
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
            worker: WorkerKind::Codex,
            model: ModelAlias::new("gpt56-codex").unwrap(),
            effort: None,
        },
        model: ModelEntry::new(WorkerKind::Codex, "gpt-5.6"),
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
async fn ac_ws13_1_golden_fixtures_map_to_the_expected_event_sequence() {
    // --- the real capture: thread.started → agent_message → command_execution
    //     (started + completed) → agent_message → turn.completed
    let (stream, events) = replay("success.jsonl");
    assert_eq!(
        kinds(&events),
        vec![
            "started",        // thread.started
            "assistant_text", // item.completed / agent_message
            "tool_call",      // item.started / command_execution
            "tool_result",    // item.completed / command_execution
            "assistant_text", // the final agent_message
            "usage",          // turn.completed.usage
            "final",          // turn.completed
        ],
        "turn.started is transcript-only"
    );
    assert!(matches!(
        &events[0],
        WorkerEvent::Started { session_id: Some(id), pid: Some(1234) }
            if id.as_str() == "01a00000-0000-7000-8000-00000000c0de"
    ));
    assert!(matches!(
        &events[2],
        WorkerEvent::ToolCall { name, input_summary }
            if name == "command_execution" && input_summary.contains("sed -n '1,200p' main.rs")
    ));
    assert!(matches!(
        &events[3],
        WorkerEvent::ToolResult { name, ok: true, output_summary }
            if name == "command_execution" && output_summary.contains("fn main()")
    ));
    assert!(matches!(&events[4], WorkerEvent::AssistantText { delta } if delta == "kevin"));
    assert!(matches!(&events[6], WorkerEvent::Final { text, .. } if text == "kevin"));
    assert!(stream.saw_final());
    assert_eq!(stream.malformed_lines(), 0);
    assert_eq!(stream.agent_message(), "kevin");
    assert!(check_contract(&events).is_ok());

    // --- the same stream driven end to end through the supervisor. The shim
    //     writes no `-o` file, so the driver falls back to the last
    //     `agent_message` — exactly what the plan's fallback is for.
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
        "01a00000-0000-7000-8000-00000000c0de"
    );
    assert!(
        transcript.bytes > 0,
        "the raw stream is kept as a transcript"
    );
    assert!(Path::new(transcript.uri.trim_start_matches("file://")).is_file());

    // --- every other item type (reasoning, web_search, todo_list, a failing
    //     command, file_change, a failing mcp_tool_call)
    let (_, events) = replay("tools.jsonl");
    assert_eq!(
        kinds(&events),
        vec![
            "started",
            "thinking",
            "tool_call",   // web_search (item.started)
            "tool_result", // web_search (item.completed)
            "tool_call",   // todo_list — completed only, so both events
            "tool_result",
            "tool_call",   // command_execution (item.started)
            "tool_result", // exit_code 101 → ok = false
            "tool_call",   // file_change — completed only
            "tool_result",
            "tool_call",   // mcp_tool_call — completed only
            "tool_result", // status "failed" → ok = false
            "assistant_text",
            "usage",
            "final",
        ]
    );
    assert!(matches!(&events[1], WorkerEvent::Thinking { delta } if delta.contains("Locating")));
    assert!(matches!(
        &events[7],
        WorkerEvent::ToolResult { name, ok: false, .. } if name == "command_execution"
    ));
    assert!(matches!(
        &events[8],
        WorkerEvent::ToolCall { name, input_summary }
            if name == "file_change" && input_summary == "update src/auth.rs, add src/auth/hash.rs"
    ));
    assert!(matches!(
        &events[11],
        WorkerEvent::ToolResult { name, ok: false, .. } if name == "mcp_tool_call"
    ));

    // --- errors
    let (stream, events) = replay("turn_failed.jsonl");
    assert_eq!(kinds(&events), vec!["started", "failed"]);
    assert!(matches!(
        &events[1],
        WorkerEvent::Failed { class: FailureClass::Permanent, message, .. }
            if message.contains("maximum number of tool calls")
    ));
    assert!(!stream.saw_final());

    let (_, events) = replay("rate_limit.jsonl");
    assert_eq!(kinds(&events), vec!["started", "failed"]);
    assert!(matches!(
        &events[1],
        WorkerEvent::Failed { class: FailureClass::Transient, message, .. }
            if message.contains("429")
    ));
}

// ---------------------------------------------------------------------------
// (2) usage extraction — and the documented absence of cost
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws13_2_usage_is_extracted_and_cost_is_documented_absent() {
    let (stream, events) = replay("success.jsonl");
    let WorkerEvent::Final { usage, .. } = events.last().unwrap() else {
        panic!("expected Final");
    };
    // `turn.completed.usage` of the captured stream was
    // {input 31509, cached_input 26112, cache_write 0, output 112, reasoning 0}.
    // Codex counts the cached tokens inside `input_tokens`; Kevin keeps the two
    // disjoint so `total_tokens()` does not double count.
    assert_eq!(usage.input_tokens, 31509 - 26112);
    assert_eq!(usage.cache_read_tokens, 26112);
    assert_eq!(usage.cache_write_tokens, 0);
    assert_eq!(usage.output_tokens, 112);
    assert_eq!(usage.total_tokens(), 5397 + 112);
    // Codex reports no price anywhere in the stream: cost stays `None` and the
    // router price table decides (`plan/04-workers.md` §Usage, cost, …).
    assert_eq!(usage.cost_usd, None);
    assert_eq!(stream.usage(), usage);

    // One `Usage` event per turn, carrying the whole delta.
    let deltas: Vec<&Usage> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Usage { delta } => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].output_tokens, 112);
    assert!(deltas[0].cost_usd.is_none());

    // `cache_write_input_tokens` and `reasoning_output_tokens` (already part of
    // `output_tokens`) are handled too.
    let (_, events) = replay("tools.jsonl");
    let WorkerEvent::Final { usage, .. } = events.last().unwrap() else {
        panic!("expected Final");
    };
    assert_eq!(usage.input_tokens, 4200 - 1200);
    assert_eq!(usage.cache_read_tokens, 1200);
    assert_eq!(usage.cache_write_tokens, 300);
    assert_eq!(usage.output_tokens, 640);

    // End to end the outcome carries the same usage, plus a wall clock.
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let outcome = worker
        .start(request(dir.path()))
        .await
        .expect("spawn")
        .wait()
        .await;
    assert_eq!(outcome.usage().total_tokens(), 5509);
    assert_eq!(outcome.usage().cost_usd, None);
}

// ---------------------------------------------------------------------------
// (3) sandbox / bypass policy enforced
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws13_3_bypass_and_danger_sandbox_are_rejected_under_cli_native() {
    let dir = tempfile::tempdir().unwrap();

    // (a) sandbox = danger-full-access
    let danger = CodexConfig {
        bin: shim().to_string_lossy().into_owned(),
        sandbox: CodexSandbox::DangerFullAccess,
        ..CodexConfig::default()
    };
    let native = CodexWorker::new(
        danger.clone(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let err = native.start(request(dir.path())).await.unwrap_err();
    assert!(
        matches!(&err, WorkerError::PolicyViolation { flag, tier }
            if flag == "danger-full-access" && tier == "cli-native"),
        "{err}"
    );

    // (b) `--dangerously-bypass-approvals-and-sandbox` smuggled through extra_args
    let smuggled = CodexConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
        ..CodexConfig::default()
    };
    let native = CodexWorker::new(
        smuggled.clone(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let err = native.start(request(dir.path())).await.unwrap_err();
    assert!(
        matches!(&err, WorkerError::PolicyViolation { flag, .. }
            if flag == "--dangerously-bypass-approvals-and-sandbox"),
        "{err}"
    );

    // (c) the container tier allows both.
    for cfg in [danger, smuggled] {
        let container = CodexWorker::new(
            cfg,
            SandboxPolicy::container(),
            Duration::from_secs(1),
            dir.path(),
        );
        assert!(container.build_argv(&request(dir.path()), None).is_ok());
    }

    // (d) a read-only task always gets `-s read-only`, whatever is configured.
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    req.spec.workspace_policy = kevin_worker::WorkspacePolicy::ReadOnly;
    let argv = worker.build_argv(&req, None).unwrap();
    assert!(argv.windows(2).any(|w| w == ["-s", "read-only"]));

    // (e) the registry refuses to build a danger-full-access config outside
    //     container, and accepts it inside.
    let mut cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    };
    cfg.codex.sandbox = CodexSandbox::DangerFullAccess;
    let errs =
        WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).expect_err("must reject");
    assert!(errs.to_string().contains("workers.codex.sandbox"), "{errs}");
    assert!(WorkerRegistry::from_config(&cfg, SandboxPolicy::container()).is_ok());
}

// ---------------------------------------------------------------------------
// (4) doctor detects binary + auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws13_4_doctor_detects_binary_and_auth() {
    let dir = tempfile::tempdir().unwrap();

    // The shim answers `--version`, so the row is complete except for auth,
    // which is read from `$CODEX_HOME/auth.json` / `OPENAI_API_KEY` and never
    // by calling the API.
    let worker = shim_worker("success.jsonl", dir.path(), SandboxPolicy::cli_native());
    let doctor = worker.doctor().await;
    assert_eq!(doctor.kind, WorkerKind::Codex);
    assert!(doctor.binary.is_some());
    assert!(doctor.version.unwrap().starts_with("fake-cli"));
    assert!(
        matches!(
            doctor.auth_ready,
            AuthStatus::Ready | AuthStatus::Missing(_)
        ),
        "auth readiness is decided offline, never `Unknown`"
    );

    // A missing binary is reported, never a panic.
    let missing = CodexConfig {
        bin: "definitely-not-codex-kevin".to_owned(),
        ..CodexConfig::default()
    };
    let worker = CodexWorker::new(
        missing,
        SandboxPolicy::cli_native(),
        Duration::from_secs(1),
        dir.path(),
    );
    let doctor = worker.doctor().await;
    assert!(doctor.binary.is_none());
    assert!(!doctor.is_healthy());
    assert!(doctor.notes[0].contains("workers.codex.bin"));

    // …and `kevin workers doctor` gets a codex row from the registry, with the
    // adapter present in this build.
    let cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    };
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let codex = registry.get(WorkerKind::Codex).expect("codex registered");
    assert_eq!(codex.kind(), WorkerKind::Codex);
    for alias in cfg.aliases_for(WorkerKind::Codex) {
        codex.validate_alias(&alias, &cfg.models[&alias]).unwrap();
    }
    let doctors = registry.doctor_all().await;
    let row = doctors
        .iter()
        .find(|d| d.kind == WorkerKind::Codex)
        .expect("doctor row for codex");
    assert!(
        !row.notes
            .iter()
            .any(|n| n.contains("adapter not available"))
    );
}

// ---------------------------------------------------------------------------
// (5) live smoke test — opt-in only, never in CI
// ---------------------------------------------------------------------------

/// Runs the *real* `codex` CLI once with a trivial prompt.
///
/// Doubly gated: `#[ignore]` (nextest/cargo skip it by default) *and* an
/// explicit `KEVIN_LIVE_TESTS=1`. Run it with
/// `KEVIN_LIVE_TESTS=1 cargo nextest run -p kevin-worker --run-ignored all
/// ac_ws13_5`. The attempt runs read-only in an empty temp dir at the lowest
/// reasoning effort.
#[tokio::test]
#[ignore = "live: spends money; set KEVIN_LIVE_TESTS=1 and pass --run-ignored"]
async fn ac_ws13_5_live_smoke_runs_codex_once() {
    if std::env::var("KEVIN_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: KEVIN_LIVE_TESTS != 1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cfg = CodexConfig {
        sandbox: CodexSandbox::ReadOnly,
        ..CodexConfig::default()
    };
    let worker = CodexWorker::new(
        cfg,
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        data.path(),
    );
    let doctor = worker.doctor().await;
    assert!(
        doctor.binary.is_some(),
        "the real `codex` binary must be on PATH for the live test"
    );
    assert_eq!(
        doctor.auth_ready,
        AuthStatus::Ready,
        "the live test needs codex credentials"
    );

    let mut req = request(dir.path());
    req.spec = TaskSpec::new(
        "live smoke",
        "Reply with exactly the word: kevin. Do not run any command.",
    );
    req.route.effort = Some(Effort::Low);
    req.budget = AttemptBudget::with_timeout(Duration::from_secs(180));

    let (events, outcome) = worker.start(req).await.expect("spawn").collect().await;
    check_contract(&events).expect("stream contract");
    let WorkerOutcome::Succeeded { text, usage, .. } = &outcome else {
        panic!("live codex run failed: {outcome:?}");
    };
    assert!(
        text.to_lowercase().contains("kevin"),
        "unexpected answer: {text}"
    );
    assert!(usage.output_tokens > 0);
    assert_eq!(usage.cost_usd, None, "codex never reports cost");
}

// ---------------------------------------------------------------------------
// Supporting tests
// ---------------------------------------------------------------------------

/// `plan/11-testing.md` §Worker adapter testing (1): argv per tier ×
/// {schema, none} × {fresh, resume}.
#[test]
fn argv_snapshot_per_tier_schema_and_session() {
    let worker = CodexWorker::new(
        CodexConfig::default(),
        SandboxPolicy::cli_native(),
        Duration::from_secs(10),
        "/data",
    );
    let mut req = request(Path::new("/workspace"));
    req.attempt_id = AttemptId::nil();
    req.run_id = RunId::nil();
    req.task_id = TaskId::nil();
    let fresh = worker.build_argv(&req, None).unwrap();
    assert_eq!(
        fresh.join(" "),
        format!(
            "exec --json -m gpt-5.6 -C /workspace -s workspace-write \
             -o /data/runs/{run}/{task}/{attempt}.last.txt --skip-git-repo-check -",
            run = RunId::nil(),
            task = TaskId::nil(),
            attempt = AttemptId::nil()
        )
    );

    req.spec.output_schema = Some(json!({"type": "object", "required": ["status"]}));
    req.route.effort = Some(Effort::XHigh);
    let with_schema = worker.build_argv(&req, None).unwrap();
    let i = with_schema
        .iter()
        .position(|a| a == "--output-schema")
        .unwrap();
    assert!(with_schema[i + 1].ends_with(".schema.json"));
    assert!(
        with_schema
            .windows(2)
            .any(|w| w == ["-c", "model_reasoning_effort=xhigh"])
    );

    // `codex exec resume` takes neither `-C` nor `-s`.
    let resumed = worker.build_argv(&req, Some("th-42")).unwrap();
    assert_eq!(resumed[..3], ["exec", "resume", "th-42"]);
    assert!(!resumed.iter().any(|a| a == "-C" || a == "-s"));
    assert_eq!(resumed.last().unwrap(), "-");

    // The container tier keeps the same shape, only the sandbox differs.
    let container = CodexWorker::new(
        CodexConfig {
            sandbox: CodexSandbox::DangerFullAccess,
            ..CodexConfig::default()
        },
        SandboxPolicy::container(),
        Duration::from_secs(10),
        "/data",
    );
    assert!(
        container
            .build_argv(&request(Path::new("/workspace")), None)
            .unwrap()
            .windows(2)
            .any(|w| w == ["-s", "danger-full-access"])
    );
}

#[tokio::test]
async fn structured_output_is_extracted_from_the_final_message_and_validated() {
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("structured.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    req.spec.output_schema = Some(json!({
        "type": "object",
        "properties": {"status": {"type": "string"}, "files_changed": {"type": "integer"}},
        "required": ["status", "files_changed"]
    }));
    // The schema file is written next to the transcript before the spawn.
    let outcome = worker.start(req.clone()).await.expect("spawn").wait().await;
    assert!(worker.output_schema_path(&req).is_file());
    let WorkerOutcome::Succeeded { structured, .. } = &outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(structured.as_ref().unwrap()["status"], "ok");
    assert_eq!(structured.as_ref().unwrap()["files_changed"], 2);
}

#[tokio::test]
async fn a_schema_violation_that_cannot_be_repaired_is_permanent() {
    let dir = tempfile::tempdir().unwrap();
    let worker = shim_worker("structured.jsonl", dir.path(), SandboxPolicy::cli_native());
    let mut req = request(dir.path());
    // The fixture answers {"status":"ok","files_changed":2}; demand something
    // else. The repair turn resumes the same thread (the shim replays the same
    // fixture) and fails again → Permanent.
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
    // The repair turn resumed instead of starting a second thread: exactly one
    // `started` event reached the consumer.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, WorkerEvent::Started { .. }))
            .count(),
        1
    );
}

/// The `-o/--output-last-message` file is the authoritative final answer; the
/// last `agent_message` item is only the fallback (`plan/04-workers.md`
/// §Adapter: codex). Driven by a shim that behaves like `codex` on that point.
#[tokio::test]
async fn the_output_last_message_file_wins_over_the_last_agent_message() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("codex-shim.sh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\ncat '{}'\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = '-o' ]; then shift;              printf '%s' 'the -o answer' > \"$1\"; fi\n  shift\ndone\n",
            fixture("success.jsonl").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let worker = CodexWorker::new(
        CodexConfig {
            bin: bin.to_string_lossy().into_owned(),
            ..CodexConfig::default()
        },
        SandboxPolicy::cli_native(),
        Duration::from_secs(5),
        dir.path(),
    );
    let req = request(dir.path());
    let last = worker.last_message_path(&req);
    let outcome = worker.start(req).await.expect("spawn").wait().await;
    let WorkerOutcome::Succeeded { text, .. } = &outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(text, "the -o answer");
    assert_eq!(std::fs::read_to_string(&last).unwrap(), "the -o answer");
}

#[tokio::test]
async fn cancellation_and_a_missing_final_are_classified() {
    let dir = tempfile::tempdir().unwrap();

    // A child that never emits `turn.completed` exits 0 without a Final.
    let cfg = CodexConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--stderr".to_owned(), "nothing to do".to_owned()],
        ..CodexConfig::default()
    };
    let worker = CodexWorker::new(
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
    let cfg = CodexConfig {
        bin: shim().to_string_lossy().into_owned(),
        extra_args: vec!["--hang".to_owned()],
        ..CodexConfig::default()
    };
    let worker = CodexWorker::new(
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
    let mut stream = CodexStream::new(None);
    for line in [
        "",
        "   ",
        "null",
        "[]",
        "\"a string\"",
        "{}",
        r#"{"type":"item.completed"}"#,
        r#"{"type":"item.completed","item":{}}"#,
        r#"{"type":"item.completed","item":{"type":"agent_message"}}"#,
        r#"{"type":"item.completed","item":{"type":"file_change","changes":"nope"}}"#,
        r#"{"type":"item.started","item":{"type":"unknown_future_item"}}"#,
        r#"{"type":"turn.completed"}"#,
        r#"{"type":"turn.failed"}"#,
        r#"{"type":"unknown"}"#,
        "{\"type\":\"thread.started\"",
    ] {
        let _ = stream.parse_line(line);
    }
    assert!(stream.session_id().is_none());
}
