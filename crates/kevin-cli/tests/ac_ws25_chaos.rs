//! WS-25 hardening — a real `kill -9` on the `kevin` process (`plan/12` §WS-25).
//!
//! `ac_ws25_1_1` (kevin-orchestrator) aborts the engine's tasks in-process,
//! which is the fast version of this and covers the saga logic. It cannot
//! cover what a `SIGKILL` actually does: no destructor runs, no shutdown hook
//! fires, no event is written on the way out, and the operating system tears
//! the worker subprocesses' parent away mid-stream. That is the failure mode
//! this file reproduces, with two real processes and one real signal.
//!
//! Needs Postgres and a unix signal; skips cleanly without either.

#![cfg(unix)]

mod common;

use std::time::Duration;

use common::Harness;

/// A run whose single implement task holds forever, so the runtime can be
/// killed with an attempt genuinely in flight.
const SCENARIO: &str = r#"
default:
  reply: "done"
  usage: { input_tokens: 10, output_tokens: 5 }
rules:
  - match: "/^planner\\.understanding/"
    structured:
      objective: "Add a /healthz endpoint"
      assumptions: []
      risks: []
      success_criteria: ["GET /healthz returns 200"]
      proposed_questions: []
      complexity: "low"
      suggested_task_kinds: ["implement"]
  - match: "/^planner\\.plan/"
    structured:
      rationale: "one task is enough"
      tasks:
        - id: "t1"
          title: "Hold the healthz route"
          kind: "implement"
          instructions: "wait to be killed"
          acceptance_criteria: ["GET /healthz returns 200"]
          depends_on: []
  - match: "/^Hold the healthz route/"
    hold: true
  - match: "/^integrator/"
    structured:
      status: "skipped"
      summary: "nothing to integrate"
      merged: []
      conflicts: []
      checks: []
      artifacts: []
  - match: "/^judge/"
    structured:
      scores:
        - { criterion: "correctness", score: 9, rationale: "the route returns 200" }
        - { criterion: "completeness", score: 8, rationale: "every criterion is met" }
        - { criterion: "quality", score: 8, rationale: "small and readable" }
        - { criterion: "safety", score: 10, rationale: "no destructive change" }
        - { criterion: "efficiency", score: 7, rationale: "one attempt" }
      overall: 0.85
      verdict: "accept"
      lessons: []
      proposals: []
"#;

/// `kill -9` mid-attempt, then a fresh runtime over the same database.
///
/// The contract (`plan/02` §Task state machine, `plan/08` §1.9): the attempt
/// that died is terminalised `runtime_restarted` on the next boot, and it is
/// **not replayed** — the work the killed process was doing is never handed to
/// a worker a second time under the same attempt id.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_1_2_kill_9_mid_attempt_terminalises_on_restart_without_replay() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario_and_extra(
        SCENARIO,
        // Headless so the plan auto-approves and the run reaches an attempt
        // without an operator.
        "[kevin]\nauto_approve_plans = true\n",
    )
    .await;

    // -- the victim -----------------------------------------------------------
    let code = harness
        .signal_after(
            &["run", "--headless", "add a healthz endpoint"],
            "task.attempt_started",
            "KILL",
        )
        .await;
    assert_eq!(
        code, None,
        "SIGKILL leaves no exit code; the process handled the signal instead"
    );

    // The killed process wrote nothing on the way out: the attempt is still
    // open in the ledger. This is the precondition that makes the next step
    // meaningful.
    let before = harness.event_payloads("task.attempt_started").await;
    assert_eq!(before.len(), 1, "exactly one attempt was in flight");
    let killed_attempt = before[0]["attempt_id"].clone();
    assert!(
        harness
            .event_payloads("task.attempt_failed")
            .await
            .is_empty(),
        "the killed process must not have recorded an outcome"
    );

    // -- the survivor ---------------------------------------------------------
    // A fresh runtime over the same database: `Orchestrator::boot` sweeps the
    // attempts no process owns any more.
    let daemon = harness.serve(&["--bind", "127.0.0.1:0"]).await;
    harness
        .await_event("task.attempt_failed", Duration::from_secs(60))
        .await;

    let failures = harness.event_payloads("task.attempt_failed").await;
    assert_eq!(
        failures.len(),
        1,
        "the sweep ran more than once: {failures:?}"
    );
    assert_eq!(
        failures[0]["class"], "runtime_restarted",
        "the killed attempt must be terminalised as runtime_restarted"
    );
    assert_eq!(failures[0]["attempt_id"], killed_attempt);

    // No replay: the killed attempt id is never started again. A *retry* is
    // allowed — it gets a new attempt id — but re-running the same attempt
    // would double-charge the operator and re-apply the worker's side effects.
    let started = harness.event_payloads("task.attempt_started").await;
    assert_eq!(
        started
            .iter()
            .filter(|p| p["attempt_id"] == killed_attempt)
            .count(),
        1,
        "the killed attempt was replayed: {started:?}"
    );

    // The retry of the swept task holds forever by design, so the daemon is
    // stopped with SIGTERM and its `shutdown_grace_period` (3s in the harness
    // config) rather than waiting for a drain that cannot finish.
    daemon.signal("TERM");
    let _ = daemon.wait(Duration::from_secs(90)).await;
    harness.close().await;
}
