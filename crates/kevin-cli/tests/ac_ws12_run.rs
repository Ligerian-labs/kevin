//! WS-12 acceptance criteria (`plan/12-workstreams.md`): `kevin run` against the
//! embedded runtime with the `fake` worker.
//!
//! 1. `kevin run "…" --no-tui` completes, exits 0 and prints the event stream.
//! 2. A clarification question is asked and answered from stdin.
//! 3. `--json` emits one JSON object per event.
//! 4. Ctrl-C cancels the run (`run.cancelled` recorded) and exits 130.
//!
//! Every scenario boots the whole pipeline for real — config → migrations →
//! store → bus → projections → orchestrator → fake worker — on a per-test
//! database (`kevin_testkit::pg::TestDb`).

mod common;

use common::{Harness, run_events, task_kinds};
use predicates::prelude::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_1_run_completes_and_prints_the_event_stream() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::new().await;

    let output = harness
        .kevin(&["run", "add a /healthz endpoint", "--headless", "--no-tui"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf-8 stdout");

    for event in [
        "run.started",
        "run.understanding_completed",
        "run.plan_proposed",
        "run.plan_approved",
        "task.created",
        "task.attempt_succeeded",
        "run.integrated",
        "run.completed",
    ] {
        assert!(stdout.contains(event), "missing {event} in:\n{stdout}");
    }
    assert!(stdout.contains("completed"), "no summary in:\n{stdout}");

    // The read models the CLI printed from are the ones the API will serve.
    let runs = harness
        .kevin(&["--json", "runs", "ls"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let runs: serde_json::Value = serde_json::from_slice(&runs).expect("runs ls --json");
    let first = &runs["runs"][0];
    assert_eq!(first["status"], "completed");
    assert_eq!(first["task_counts"]["succeeded"], 1);

    let kinds = task_kinds(&harness).await;
    assert_eq!(kinds, vec!["implement".to_owned()]);
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_2_a_question_is_asked_and_answered_from_stdin() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario(common::SCENARIO_WITH_QUESTION).await;

    // Interactive mode ⇒ `QuestionPolicy::Block`; the answer arrives on stdin
    // ("2" selects the second option) and the run proceeds.
    let assert = harness
        .kevin(&["run", "add a /healthz endpoint", "--no-tui", "--yes"])
        .write_stdin("2\n")
        .assert()
        .code(0);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    assert!(
        stdout.contains("question.asked"),
        "no question in:\n{stdout}"
    );
    assert!(
        stdout.contains("question.answered"),
        "question was never answered:\n{stdout}"
    );

    let events = run_events(&harness).await;
    let answered = events
        .iter()
        .find(|(event_type, _)| *event_type == "question.answered")
        .expect("question.answered recorded");
    assert_eq!(answered.1["answer"]["selected"][0], "sqlite");
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_3_json_emits_one_object_per_event() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::new().await;

    let output = harness
        .kevin(&[
            "--json",
            "run",
            "add a /healthz endpoint",
            "--headless",
            "--no-tui",
        ])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("utf-8 stdout");

    let mut events = Vec::new();
    let mut kinds = Vec::new();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line} ({e})"));
        assert!(value.is_object(), "not a JSON object: {line}");
        let kind = value["type"]
            .as_str()
            .expect("every line has a type")
            .to_owned();
        if kind == "event" {
            assert!(value["position"].is_u64());
            assert!(value["event_id"].is_string());
            assert!(value["occurred_at"].is_string());
            assert!(value["payload"].is_object());
            events.push(value["event_type"].as_str().unwrap_or_default().to_owned());
        }
        kinds.push(kind);
    }
    assert_eq!(kinds.first().map(String::as_str), Some("run_started"));
    assert_eq!(kinds.last().map(String::as_str), Some("summary"));

    // Exactly one `event` object per event of the run in `core.events`.
    let stored: Vec<String> = run_events(&harness)
        .await
        .into_iter()
        .map(|(event_type, _)| event_type)
        .collect();
    assert_eq!(events, stored, "the JSON stream is not the event stream");
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_4_ctrl_c_cancels_the_run_and_exits_130() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario(common::SCENARIO_HOLDING).await;

    // The fake worker holds the implement attempt open, so the run is still
    // executing when SIGINT arrives.
    let code = harness
        .interrupt_after(
            &[
                "--json",
                "run",
                "add a /healthz endpoint",
                "--headless",
                "--no-tui",
            ],
            "task.attempt_started",
        )
        .await;
    assert_eq!(code, Some(130), "Ctrl-C must exit 130");

    let events = run_events(&harness).await;
    assert!(
        events.iter().any(|(t, _)| t == "run.cancelled"),
        "run.cancelled was not recorded: {:?}",
        events.iter().map(|(t, _)| t).collect::<Vec<_>>()
    );

    let shown = harness
        .kevin(&["--json", "runs", "ls"])
        .assert()
        .code(0)
        .get_output()
        .stdout
        .clone();
    let shown: serde_json::Value = serde_json::from_slice(&shown).expect("runs ls --json");
    assert_eq!(shown["runs"][0]["status"], "cancelled");
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_5_read_side_commands_answer_from_the_read_models() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::new().await;
    harness
        .kevin(&[
            "run",
            "add a /healthz endpoint",
            "--headless",
            "--no-tui",
            "-q",
        ])
        .assert()
        .code(0);

    let runs: serde_json::Value = harness.json(&["--json", "runs", "ls"]);
    let run_id = runs["runs"][0]["id"].as_str().expect("a run id").to_owned();

    let show: serde_json::Value = harness.json(&["--json", "runs", "show", &run_id]);
    assert_eq!(show["status"], "completed");
    assert_eq!(show["tasks"].as_array().map(Vec::len), Some(1));

    let tasks: serde_json::Value = harness.json(&["--json", "tasks", "ls", &run_id]);
    let task_id = tasks["tasks"][0]["id"]
        .as_str()
        .expect("a task id")
        .to_owned();
    let task: serde_json::Value = harness.json(&["--json", "tasks", "show", &task_id]);
    assert_eq!(task["status"], "succeeded");

    let cost: serde_json::Value = harness.json(&["--json", "cost", "--group-by", "model"]);
    assert!(cost["rows"].as_array().is_some_and(|r| !r.is_empty()));

    let questions: serde_json::Value = harness.json(&["--json", "questions", "ls"]);
    assert_eq!(questions["questions"].as_array().map(Vec::len), Some(0));

    harness
        .kevin(&["runs", "events", &run_id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("run.completed"));

    // `watch` on a terminal run replays it from the store and stops.
    harness
        .kevin(&["runs", "watch", &run_id])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("run.completed"));

    harness.kevin(&["tasks", "log", &task_id]).assert().code(0);

    // A terminal run cannot be cancelled: the domain refuses, exit 1.
    harness.kevin(&["runs", "cancel", &run_id]).assert().code(1);
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ac_ws12_7_a_completed_run_is_judged_and_records_an_evaluation() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::new().await;
    harness
        .kevin(&[
            "run",
            "add a /healthz endpoint",
            "--headless",
            "--no-tui",
            "-q",
        ])
        .assert()
        .code(0);

    // The real `EvaluatorPort` judged the run through the fake worker.
    let events = run_events(&harness).await;
    let recorded = events
        .iter()
        .find(|(event_type, _)| event_type == "evaluation.recorded")
        .unwrap_or_else(|| {
            panic!(
                "no evaluation.recorded in {:?}",
                events.iter().map(|(t, _)| t).collect::<Vec<_>>()
            )
        });
    assert_eq!(recorded.1["verdict"], "accept");
    assert!(
        events.iter().any(|(t, _)| t == "run.evaluated"),
        "the run was completed without an evaluation: {:?}",
        events.iter().map(|(t, _)| t).collect::<Vec<_>>()
    );

    let show: serde_json::Value = {
        let runs: serde_json::Value = harness.json(&["--json", "runs", "ls"]);
        let run_id = runs["runs"][0]["id"].as_str().expect("a run id").to_owned();
        harness.json(&["--json", "runs", "show", &run_id])
    };
    assert_eq!(show["status"], "completed");
    harness.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_ws12_6_server_mode_and_bad_arguments_use_the_documented_exit_codes() {
    let harness = Harness::offline();
    // `--server` is refused before anything touches the database (WS-16/WS-20).
    harness
        .kevin(&["--server", "http://127.0.0.1:7777", "runs", "ls"])
        .assert()
        .code(2);
    // A goal outside a repository needs --allow-plain-dir.
    harness
        .kevin_raw(&["run", "do the thing", "--no-tui"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("--allow-plain-dir"));
    // Unknown model alias.
    harness
        .kevin(&["run", "do the thing", "--no-tui", "--model", "nope"])
        .assert()
        .code(3);
}
