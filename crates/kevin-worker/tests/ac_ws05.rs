//! WS-05 acceptance tests (`plan/12-workstreams.md` §WS-05).
//!
//! Supervisor tests drive the `fake-cli` shim (`tests/bin/fake-cli.rs`); fake
//! worker tests replay the fixtures under `tests/fixtures/fake/`. No real
//! coding-agent CLI is ever invoked.

#![allow(
    clippy::unwrap_used,
    clippy::items_after_statements,
    clippy::match_wildcard_for_single_variants,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kevin_domain::{AttemptId, FailureClass, ModelAlias, RunId, TaskId, TaskKind, WorkerKind};
use kevin_worker::fake::{FakeWorker, Scenario};
use kevin_worker::registry::{RegistryConfig, WorkerRegistry};
use kevin_worker::structured::{self, StructuredError};
use kevin_worker::supervisor::{
    self, ChildHandle, ExitReason, SpawnOpts, Supervisor, Verdict, transcript_path,
};
use kevin_worker::worker::check_contract;
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, ModelEntry, Route, SandboxPolicy,
    TaskAttemptRequest, TaskSpec, Worker, WorkerEvent, WorkerOutcome, Workspace,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn shim() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-cli"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fake")
        .join(name)
}

fn spawn(args: &[&str], opts: SpawnOpts) -> ChildHandle {
    let mut cmd = Supervisor::command(shim());
    cmd.args(args);
    Supervisor::spawn(cmd, opts).expect("spawn shim")
}

fn opts(dir: &Path) -> SpawnOpts {
    SpawnOpts::new(WorkerKind::Fake, dir).env(BTreeMap::from([(
        "PATH".to_owned(),
        std::env::var("PATH").unwrap_or_default(),
    )]))
}

fn pid_alive(pid: u32) -> bool {
    let raw = i32::try_from(pid).expect("pid fits i32");
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None).is_ok()
}

async fn wait_dead(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    !pid_alive(pid)
}

fn request(prompt: &str, workspace: &Path) -> TaskAttemptRequest {
    TaskAttemptRequest {
        attempt_id: AttemptId::new(),
        task_id: TaskId::new(),
        run_id: RunId::new(),
        kind: TaskKind::Implement,
        spec: TaskSpec::new("task", prompt),
        route: Route {
            worker: WorkerKind::Fake,
            model: ModelAlias::new("fake").unwrap(),
            effort: None,
        },
        model: ModelEntry::new(WorkerKind::Fake, "fake"),
        workspace: Workspace::in_place(workspace),
        context: AttemptContext::default(),
        env: EnvAllowlist::new(["PATH"]),
        budget: AttemptBudget::with_timeout(Duration::from_secs(30)),
        cancel: CancellationToken::new(),
    }
}

// ---------------------------------------------------------------------------
// (1) cancel kills the whole process group within kill_grace
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_1_cancel_kills_process_group_within_kill_grace() {
    let dir = tempfile::tempdir().unwrap();
    let kill_grace = Duration::from_millis(500);
    let mut child = spawn(
        &["--spawn-child", "--ignore-sigterm"],
        opts(dir.path()).kill_grace(kill_grace),
    );
    let leader = child.pid();
    let first = child.next_line().await.expect("child_pid line");
    let grandchild: u32 = first
        .text
        .strip_prefix("child_pid=")
        .expect("child_pid= prefix")
        .parse()
        .unwrap();
    assert!(pid_alive(leader));
    assert!(pid_alive(grandchild));

    let started = Instant::now();
    child.cancel();
    let exit = child.wait().await;
    let elapsed = started.elapsed();

    assert_eq!(exit.reason, ExitReason::Cancelled);
    assert!(
        elapsed < kill_grace + Duration::from_secs(2),
        "cancel took {elapsed:?}, kill_grace {kill_grace:?}"
    );
    assert!(
        wait_dead(leader, Duration::from_secs(2)).await,
        "leader still alive"
    );
    assert!(
        wait_dead(grandchild, Duration::from_secs(2)).await,
        "grandchild {grandchild} escaped the process-group kill"
    );
    match supervisor::classify(&exit, false) {
        Verdict::Failed { class, .. } => assert_eq!(class, FailureClass::Cancelled),
        Verdict::Succeeded => panic!("cancelled child must not succeed"),
    }
}

// ---------------------------------------------------------------------------
// (2) timeout → Failed{Transient}; non-zero exit classes table
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_2_timeout_is_transient_and_exit_classes_follow_the_table() {
    let dir = tempfile::tempdir().unwrap();

    // Timeout.
    let started = Instant::now();
    let child = spawn(
        &["--hang"],
        opts(dir.path())
            .timeout(Duration::from_millis(300))
            .kill_grace(Duration::from_millis(200)),
    );
    let pid = child.pid();
    let exit = child.wait().await;
    assert_eq!(exit.reason, ExitReason::Timeout);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(wait_dead(pid, Duration::from_secs(2)).await);
    assert_eq!(
        supervisor::classify(&exit, true),
        Verdict::Failed {
            class: FailureClass::Transient,
            message: "timeout".to_owned()
        }
    );

    // Exit-class table through real processes.
    struct Case {
        args: &'static [&'static str],
        saw_final: bool,
        expect: Option<FailureClass>,
        message_contains: &'static str,
    }
    let cases = [
        Case {
            args: &["--exit", "0"],
            saw_final: true,
            expect: None,
            message_contains: "",
        },
        Case {
            args: &["--exit", "0"],
            saw_final: false,
            expect: Some(FailureClass::Permanent),
            message_contains: "no final message",
        },
        Case {
            args: &["--exit", "1", "--stderr", "fatal: bad config"],
            saw_final: true,
            expect: Some(FailureClass::Permanent),
            message_contains: "exit 1: fatal: bad config",
        },
        Case {
            args: &["--exit", "1", "--stderr", "HTTP 429 Too Many Requests"],
            saw_final: false,
            expect: Some(FailureClass::Transient),
            message_contains: "429",
        },
        Case {
            args: &["--exit", "2", "--stderr", "Error: read ECONNRESET"],
            saw_final: false,
            expect: Some(FailureClass::Transient),
            message_contains: "ECONNRESET",
        },
        Case {
            args: &["--exit", "1", "--stderr", "api is overloaded, retry later"],
            saw_final: false,
            expect: Some(FailureClass::Transient),
            message_contains: "overloaded",
        },
        Case {
            args: &["--exit", "137"],
            saw_final: false,
            expect: Some(FailureClass::Transient),
            message_contains: "exit 137",
        },
        Case {
            args: &["--abort"],
            saw_final: false,
            expect: Some(FailureClass::Transient),
            message_contains: "killed by signal",
        },
    ];
    for case in cases {
        let exit = spawn(case.args, opts(dir.path())).wait().await;
        let verdict = supervisor::classify(&exit, case.saw_final);
        match (case.expect, &verdict) {
            (None, Verdict::Succeeded) => {}
            (
                Some(class),
                Verdict::Failed {
                    class: got,
                    message,
                },
            ) => {
                assert_eq!(*got, class, "{:?}: {verdict:?}", case.args);
                assert!(
                    message.contains(case.message_contains),
                    "{:?}: message {message:?} lacks {:?}",
                    case.args,
                    case.message_contains
                );
            }
            _ => panic!("{:?} saw_final={} → {verdict:?}", case.args, case.saw_final),
        }
    }
}

// ---------------------------------------------------------------------------
// (3) 10 MB of stdout with a slow consumer never exceeds the bounded buffer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_3_stdout_flood_with_slow_consumer_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    const LINES: u64 = 10 * 1024;
    const LINE_BYTES: u64 = 1024; // incl. newline → 10 MiB total
    const CAPACITY: u64 = 256;
    let mut child = spawn(
        &["--lines", "10240", "--bytes", "1024"],
        opts(dir.path()).line_capacity(CAPACITY as usize),
    );
    // Bound: bytes the reader may be ahead of the consumer = channel capacity
    // + one in-flight line + the pipe reader's buffer (64 KiB) + stderr noise.
    let bound = (CAPACITY + 2) * LINE_BYTES + 64 * 1024 + 4096;

    let mut consumed_bytes = 0u64;
    let mut consumed_lines = 0u64;
    for round in 0..5 {
        for _ in 0..64 {
            let line = child.next_line().await.expect("line");
            consumed_lines += 1;
            consumed_bytes += line.text.len() as u64 + 1;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let stats = child.stats();
        assert!(
            stats.bytes_read <= consumed_bytes + bound,
            "round {round}: reader ran ahead: read {} consumed {consumed_bytes} bound {bound}",
            stats.bytes_read
        );
        assert!(stats.bytes_read >= consumed_bytes);
    }
    while let Some(line) = child.next_line().await {
        consumed_lines += 1;
        consumed_bytes += line.text.len() as u64 + 1;
        assert!(!line.truncated);
    }
    assert_eq!(consumed_lines, LINES);
    assert_eq!(consumed_bytes, LINES * LINE_BYTES);
    let exit = child.wait().await;
    assert_eq!(exit.reason, ExitReason::Exited(0));
}

#[tokio::test]
async fn supervisor_truncates_overlong_lines_and_counts_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn(
        &["--lines", "3", "--bytes", "5000"],
        opts(dir.path()).max_line_bytes(100),
    );
    let mut lines = Vec::new();
    while let Some(line) = child.next_line().await {
        lines.push(line);
    }
    assert_eq!(lines.len(), 3);
    assert!(lines.iter().all(|l| l.truncated && l.text.len() == 100));
    let stats = child.stats();
    assert_eq!(stats.lines_truncated, 3);
    let exit = child.wait().await;
    assert!(exit.success());
}

// ---------------------------------------------------------------------------
// (4) fake worker replays a scenario incl. [[KOHRAL_HOLD]] and kohral-ok
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_4_fake_worker_replays_scenario_with_kohral_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let scenario = Scenario::load(fixture("plan.yaml")).unwrap();
    let worker = FakeWorker::new(scenario, dir.path());
    assert_eq!(worker.kind(), WorkerKind::Fake);

    // reply deterministically → exactly `kohral-ok`
    let req = request("Please reply deterministically.", dir.path());
    let (events, outcome) = worker.start(req.clone()).await.unwrap().collect().await;
    check_contract(&events).unwrap();
    match &outcome {
        WorkerOutcome::Succeeded {
            text,
            structured,
            session_id,
            transcript,
            ..
        } => {
            assert_eq!(text, "kohral-ok");
            assert!(structured.is_none());
            assert_eq!(
                session_id.as_ref().map(|s| s.as_str().to_owned()),
                Some(format!("fake-{}", req.attempt_id))
            );
            let expected = transcript_path(dir.path(), &req.run_id, &req.task_id, &req.attempt_id);
            assert_eq!(transcript.uri, format!("file://{}", expected.display()));
            let body = std::fs::read_to_string(&expected).unwrap();
            assert_eq!(body.lines().count(), 2, "{body}");
            assert_eq!(transcript.bytes, body.len() as u64);
            assert_eq!(transcript.sha256, supervisor::sha256_hex(body.as_bytes()));
        }
        other => panic!("{other:?}"),
    }

    // [[KOHRAL_HOLD]] → Started, then nothing until cancelled → Failed{Cancelled}
    let req = request("[[KOHRAL_HOLD]] please", dir.path());
    let mut handle = worker.start(req.clone()).await.unwrap();
    let first = handle.next_event().await.unwrap();
    assert!(matches!(first, WorkerEvent::Started { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), handle.next_event())
            .await
            .is_err(),
        "hold must not terminate on its own"
    );
    handle.cancel();
    let outcome = handle.wait().await;
    assert_eq!(outcome.failure_class(), Some(FailureClass::Cancelled));

    // hold + short budget → Failed{Transient, timeout}
    let mut req = request("[[KOHRAL_HOLD]]", dir.path());
    req.budget.timeout = Duration::from_millis(100);
    let (events, outcome) = worker.start(req).await.unwrap().collect().await;
    check_contract(&events).unwrap();
    match outcome {
        WorkerOutcome::Failed { class, message, .. } => {
            assert_eq!(class, FailureClass::Transient);
            assert_eq!(message, "timeout");
        }
        other => panic!("{other:?}"),
    }

    // /implement .* auth/ → scripted events, structured output, delay
    let started = Instant::now();
    let (events, outcome) = worker
        .start(request("implement the auth module", dir.path()))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(started.elapsed() >= Duration::from_millis(50));
    check_contract(&events).unwrap();
    let kinds: Vec<&str> = events.iter().map(WorkerEvent::kind_name).collect();
    assert_eq!(
        kinds,
        vec!["started", "tool_call", "assistant_text", "final"]
    );
    assert_eq!(
        events[1],
        WorkerEvent::ToolCall {
            name: "edit".into(),
            input_summary: "src/auth.rs".into()
        }
    );
    match outcome {
        WorkerOutcome::Succeeded { structured, .. } => {
            assert_eq!(structured, Some(json!({"status": "ok"})));
        }
        other => panic!("{other:?}"),
    }

    // fail transient
    let (events, outcome) = worker
        .start(request("fail transient", dir.path()))
        .await
        .unwrap()
        .collect()
        .await;
    check_contract(&events).unwrap();
    match outcome {
        WorkerOutcome::Failed {
            class,
            message,
            transcript,
            ..
        } => {
            assert_eq!(class, FailureClass::Transient);
            assert_eq!(message, "simulated 429");
            assert!(transcript.is_some());
        }
        other => panic!("{other:?}"),
    }

    // default
    let (_, outcome) = worker
        .start(request("anything else", dir.path()))
        .await
        .unwrap()
        .collect()
        .await;
    match outcome {
        WorkerOutcome::Succeeded { text, usage, .. } => {
            assert_eq!(text, "done");
            assert_eq!((usage.input_tokens, usage.output_tokens), (10, 5));
        }
        other => panic!("{other:?}"),
    }

    // Built-in scenario (no script) carries the same hooks.
    let builtin = FakeWorker::builtin(dir.path());
    let (_, outcome) = builtin
        .start(request("reply deterministically", dir.path()))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(outcome, WorkerOutcome::Succeeded { ref text, .. } if text == "kohral-ok"));
}

#[tokio::test]
async fn fake_worker_cancelled_before_start_fails_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let worker = FakeWorker::builtin(dir.path());
    let req = request("hello", dir.path());
    req.cancel.cancel();
    let (events, outcome) = worker.start(req).await.unwrap().collect().await;
    check_contract(&events).unwrap();
    assert_eq!(outcome.failure_class(), Some(FailureClass::Cancelled));
}

// ---------------------------------------------------------------------------
// (5) doctor reports missing binaries without panicking (registry level; the
//     `kevin workers doctor` command is covered in kevin-cli)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_5_registry_doctor_reports_missing_binaries_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RegistryConfig {
        data_dir: dir.path().to_path_buf(),
        ..RegistryConfig::default()
    }
    .with_bin(WorkerKind::Claude, "kevin-test-no-such-claude")
    .with_bin(
        WorkerKind::Codex,
        dir.path().join("no-such-codex").display().to_string(),
    )
    .with_bin(WorkerKind::Pi, "")
    .with_bin(WorkerKind::Opencode, shim().display().to_string())
    .enable(WorkerKind::Fake, true);
    let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
    let doctors = registry.doctor_all().await;
    let by_kind: BTreeMap<WorkerKind, _> = doctors.into_iter().map(|d| (d.kind, d)).collect();
    assert_eq!(by_kind.len(), 5);
    for kind in [WorkerKind::Claude, WorkerKind::Codex, WorkerKind::Pi] {
        let d = &by_kind[&kind];
        assert!(d.binary.is_none(), "{kind}: {d:?}");
        assert!(!d.is_healthy());
        assert!(d.notes.iter().any(|n| n.contains("missing")), "{d:?}");
    }
    let opencode = &by_kind[&WorkerKind::Opencode];
    assert_eq!(opencode.binary.as_deref(), Some(shim().as_path()));
    assert!(
        opencode
            .version
            .as_deref()
            .is_some_and(|v| v.starts_with("fake-cli ")),
        "{opencode:?}"
    );
    assert!(by_kind[&WorkerKind::Fake].is_healthy());
}

// ---------------------------------------------------------------------------
// (6) structured extraction repairs fenced JSON and rejects schema violations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws05_6_structured_extraction_repairs_fenced_json_and_rejects_violations() {
    let schema = json!({
        "type": "object",
        "required": ["status"],
        "properties": {
            "status": { "type": "string", "enum": ["ok", "error"] },
            "files": { "type": "array", "items": { "type": "string" } }
        },
        "additionalProperties": false
    });

    // Pure function: fenced + trailing commas + prose → repaired value.
    let text = "Sure:\n```json\n{\"status\": \"ok\", \"files\": [\"a.rs\",],}\n```\nbye";
    assert_eq!(
        structured::extract_and_validate(text, &schema).unwrap(),
        json!({"status": "ok", "files": ["a.rs"]})
    );
    let err = structured::extract_and_validate("{\"status\": \"maybe\"}", &schema).unwrap_err();
    assert!(matches!(err, StructuredError::SchemaViolation { .. }));
    assert!(err.to_string().starts_with("schema_violation"));
    assert_eq!(
        structured::extract_and_validate("nothing here", &schema),
        Err(StructuredError::NotFound)
    );

    // Through the fake worker with spec.output_schema.
    let dir = tempfile::tempdir().unwrap();
    let worker = FakeWorker::new(
        Scenario::load(fixture("structured.yaml")).unwrap(),
        dir.path(),
    );
    let with_schema = |prompt: &str| {
        let mut req = request(prompt, dir.path());
        req.spec = req.spec.with_output_schema(schema.clone());
        req
    };

    let (_, outcome) = worker
        .start(with_schema("fenced"))
        .await
        .unwrap()
        .collect()
        .await;
    match outcome {
        WorkerOutcome::Succeeded { structured, .. } => {
            assert_eq!(
                structured,
                Some(json!({"status": "ok", "files": ["src/auth.rs"]}))
            );
        }
        other => panic!("{other:?}"),
    }

    let (events, outcome) = worker
        .start(with_schema("violate"))
        .await
        .unwrap()
        .collect()
        .await;
    check_contract(&events).unwrap();
    match outcome {
        WorkerOutcome::Failed { class, message, .. } => {
            assert_eq!(class, FailureClass::Permanent);
            assert!(message.starts_with("schema_violation"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    // Native structured output is validated too (passes here).
    let (_, outcome) = worker
        .start(with_schema("native"))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(matches!(
        outcome,
        WorkerOutcome::Succeeded {
            structured: Some(_),
            ..
        }
    ));

    // No JSON at all → Permanent as well; without a schema the text is fine.
    let (_, outcome) = worker
        .start(with_schema("nothing"))
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(outcome.failure_class(), Some(FailureClass::Permanent));
    let (_, outcome) = worker
        .start(request("nothing", dir.path()))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(outcome.is_success());
}

// ---------------------------------------------------------------------------
// Supporting supervisor behaviour: env allow-list, cwd, stdin, transcript
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_applies_env_allowlist_cwd_stdin_and_writes_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let transcript = dir.path().join("runs/r/t/a.jsonl");
    let mut env = EnvAllowlist::new(["PATH", "HOME"]).resolve();
    env.insert("KEVIN_RUN_ID".to_owned(), "run-1".to_owned());
    let mut child = spawn(
        &[
            "--echo-env",
            "--print-cwd",
            "--print-stdin",
            "--stderr",
            "warned",
        ],
        SpawnOpts::new(WorkerKind::Fake, dir.path())
            .env(env)
            .stdin("prompt line 1\nprompt line 2\n")
            .transcript(&transcript),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(line) = child.next_line().await {
        match line.stream {
            supervisor::Stream::Stdout => stdout.push(line.text),
            supervisor::Stream::Stderr => stderr.push(line.text),
        }
    }
    let exit = child.wait().await;
    assert!(exit.success(), "{exit:?}");
    assert_eq!(exit.stderr_tail, "warned");

    let canonical = dir.path().canonicalize().unwrap();
    assert!(
        stdout
            .iter()
            .any(|l| l == &format!("cwd={}", canonical.display())),
        "{stdout:?}"
    );
    assert!(stdout.iter().any(|l| l.starts_with("PATH=")));
    assert!(stdout.contains(&"KEVIN_RUN_ID=run-1".to_owned()));
    assert!(
        !stdout.iter().any(|l| l.starts_with("CARGO_MANIFEST_DIR=")),
        "non allow-listed variable leaked into the child"
    );
    assert!(stdout.contains(&"stdin:prompt line 1".to_owned()));
    assert!(stdout.contains(&"stdin:prompt line 2".to_owned()));

    let artifact = exit.transcript.expect("transcript artifact");
    let body = std::fs::read_to_string(&transcript).unwrap();
    assert_eq!(artifact.bytes, body.len() as u64);
    assert_eq!(artifact.sha256, supervisor::sha256_hex(body.as_bytes()));
    assert_eq!(artifact.uri, format!("file://{}", transcript.display()));
    let records: Vec<serde_json::Value> = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        records
            .iter()
            .any(|r| r["stream"] == "stderr" && r["line"] == "warned")
    );
    assert!(
        records
            .iter()
            .any(|r| r["stream"] == "stdout" && r["line"] == "stdin:prompt line 1")
    );
    assert!(records.iter().all(|r| r["ts"].is_string()));
}

#[tokio::test]
async fn supervisor_reports_missing_binary_and_bad_cwd_as_spawn_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = Supervisor::spawn(
        Supervisor::command("kevin-no-such-binary-xyz"),
        opts(dir.path()),
    )
    .expect_err("missing binary must not spawn");
    assert!(
        matches!(err, kevin_worker::WorkerError::BinaryMissing { .. }),
        "{err}"
    );
    let err = Supervisor::spawn(
        Supervisor::command(shim()),
        SpawnOpts::new(WorkerKind::Fake, dir.path().join("nope")),
    )
    .expect_err("bad cwd must not spawn");
    assert!(
        matches!(err, kevin_worker::WorkerError::WorkspaceUnavailable { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn supervisor_replays_fixture_lines_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture("stream.jsonl");
    let mut child = spawn(&["--fixture", path.to_str().unwrap()], opts(dir.path()));
    let mut lines = Vec::new();
    while let Some(line) = child.next_line().await {
        lines.push(line.text);
    }
    let expected: Vec<String> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines, expected);
    assert!(child.wait().await.success());
}
