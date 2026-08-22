//! WS-08 acceptance scenarios — the fake-worker test plan of
//! `plan/05-orchestration.md` §6.
//!
//! Every scenario boots a real orchestrator (event store on a per-test
//! Postgres database, in-process bus, `fake` worker, scripted ports) and
//! asserts the **event sequence of the run**, which is the contract the plan
//! pins down. No coding-agent CLI is ever invoked.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    Harness, HoldOnce, Services, Setup, add_question, assert_order, count, default_budget,
    plan_chain, plan_of, plan_with_cycle, understanding, understanding_with_question,
};
use kevin_domain::run::{RecordUnderstanding, RunEvaluation, StartUnderstanding};
use kevin_domain::{
    EvaluationId, FailureClass, IdGen, ModelAlias, QuestionId, Route, RunMode, Usage, Verdict,
    WorkerKind,
};
use kevin_orchestrator::testing::{
    FlakyWorker, RecordingMemory, ScriptedEvaluator, ScriptedRoles, TempWorkspaces,
};
use kevin_store::StoredEvent;
use kevin_worker::fake::{FakeWorker, Rule, Scenario, ScriptedEvent};
use kevin_worker::{Usage as WorkerUsage, Worker};

fn types(events: &[StoredEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.envelope.event_type).collect()
}

fn accepted() -> RunEvaluation {
    RunEvaluation {
        evaluation_id: EvaluationId::new(),
        overall: 0.9,
        verdict: Verdict::Accept,
    }
}

/// Highest number of attempts in flight at any point of the stream.
fn peak_concurrency(events: &[StoredEvent]) -> usize {
    let mut running = 0usize;
    let mut peak = 0usize;
    for event in events {
        match event.envelope.event_type {
            "task.attempt_started" => {
                running += 1;
                peak = peak.max(running);
            }
            "task.attempt_succeeded" | "task.attempt_failed" => running = running.saturating_sub(1),
            _ => {}
        }
    }
    peak
}

/// Model aliases recorded on `task.routed`, in order.
fn routed_aliases(events: &[StoredEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.envelope.event_type == "task.routed")
        .filter_map(|e| {
            e.envelope.payload["route"]["model"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect()
}

/// The `reason` of the run's `run.failed` event.
fn failure_reason(events: &[StoredEvent]) -> String {
    events
        .iter()
        .find(|e| e.envelope.event_type == "run.failed")
        .and_then(|e| e.envelope.payload["reason"].as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("no run.failed in {:?}", types(events)))
}

// ---------------------------------------------------------------------------
// 1 — happy path
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_1_happy_path_no_questions() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("ship the feature"))
            .with_plan(plan_of(2))
            .with_summary("merged two branches"),
    );
    let harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .evaluator(Arc::new(ScriptedEvaluator::new(Some(accepted())))),
    )
    .await;
    let run = harness.start("ship the feature", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &[
            "run.started",
            "run.understanding_started",
            "run.understanding_completed",
            "run.plan_proposed",
            "run.plan_approved",
            "task.created",
            "run.execution_started",
            "task.routed",
            "task.attempt_started",
            "task.attempt_succeeded",
            "run.integrated",
            "run.evaluated",
            "run.completed",
        ],
    );
    assert_eq!(count(&events, "task.created"), 2, "{seen:?}");
    assert_eq!(count(&events, "task.attempt_succeeded"), 2, "{seen:?}");
    assert_eq!(count(&events, "question.asked"), 0, "{seen:?}");
    assert_eq!(count(&events, "run.completed"), 1, "{seen:?}");
    assert_eq!(harness.workspaces.prepared(), 2);
    assert_eq!(harness.workspaces.cleaned(), 2);
    assert_eq!(harness.router.outcomes().len(), 2);
    assert!(harness.router.outcomes().iter().all(|o| o.success));
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 2 — clarification before planning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_2_questions_then_plan() {
    kevin_testkit::skip_unless_pg!();
    let understanding = add_question(
        understanding_with_question(
            "add auth",
            "Which database?",
            &["postgres", "sqlite"],
            None,
            0.2,
        ),
        "Which framework?",
        &["axum", "actix"],
        None,
        0.3,
    );
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding)
            .with_plan(plan_of(1)),
    );
    let harness = Harness::boot(Setup::new().roles(roles)).await;
    let run = harness.start("add auth", RunMode::Interactive).await;

    let asked = harness.wait_for_n(run, "question.asked", 2).await;
    assert_eq!(count(&asked, "run.plan_proposed"), 0, "planning must wait");
    let questions: Vec<QuestionId> = harness.questions(run).await;
    harness.answer(run, questions[0], "postgres").await;
    let after_first = harness.wait_for(run, "question.answered").await;
    assert_eq!(
        count(&after_first, "run.plan_proposed"),
        0,
        "one open question still blocks planning"
    );
    harness.answer(run, questions[1], "axum").await;

    let events = harness.wait_for(run, "run.plan_proposed").await;
    let seen = types(&events);
    assert_order(
        &seen,
        &[
            "run.understanding_completed",
            "question.asked",
            "question.asked",
            "question.answered",
            "question.answered",
            "run.plan_proposed",
        ],
    );
    assert_eq!(count(&events, "run.question_answered"), 2, "{seen:?}");
    let contexts = harness.roles.plan_contexts();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].answers.len(), 2, "the planner sees the answers");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 3 — headless defaults
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_3_headless_default_answers() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding_with_question(
                "add auth",
                "Which database?",
                &["postgres", "sqlite"],
                Some("postgres"),
                0.2,
            ))
            .with_plan(plan_of(1)),
    );
    let harness = Harness::boot(Setup::new().roles(roles)).await;
    let run = harness.start("add auth", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &["question.asked", "question.answered", "run.plan_proposed"],
    );
    let answered = events
        .iter()
        .find(|e| e.envelope.event_type == "question.answered")
        .expect("question.answered");
    assert_eq!(answered.envelope.payload["answered_by"], "default");
    assert_eq!(
        answered.envelope.payload["answer"]["selected"][0],
        "postgres"
    );
    assert_eq!(count(&events, "run.completed"), 1, "{seen:?}");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 4 — a question that expires without a default fails the run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_4_question_expired_no_default() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding_with_question(
                "add auth",
                "Which database?",
                &["postgres", "sqlite"],
                None,
                0.2,
            ))
            .with_plan(plan_of(1)),
    );
    let harness = Harness::boot(Setup::new().roles(roles).config(|config| {
        config.orchestrator.question_default_timeout = Duration::from_millis(80);
    }))
    .await;
    let run = harness.start("add auth", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(&seen, &["question.asked", "question.expired", "run.failed"]);
    assert_eq!(count(&events, "question.answered"), 0, "{seen:?}");
    assert_eq!(failure_reason(&events), "unanswered_question");
    assert_eq!(count(&events, "run.plan_proposed"), 0, "{seen:?}");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5 — plan rejection loop
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_5_plan_rejected_then_revised() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("refactor"))
            .with_plan(plan_of(1))
            .with_plan(plan_of(2)),
    );
    let harness = Harness::boot(Setup::new().roles(roles)).await;
    let run = harness.start("refactor", RunMode::Interactive).await;

    harness.wait_for(run, "run.plan_proposed").await;
    harness.reject(run, "split the work in two").await;
    let revised = harness
        .wait_until(run, "a second run.plan_proposed", |events| {
            count(events, "run.plan_proposed") >= 2
        })
        .await;
    harness.approve(run).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &[
            "run.plan_proposed",
            "run.plan_rejected",
            "run.plan_proposed",
            "run.plan_approved",
            "run.completed",
        ],
    );
    assert_eq!(count(&revised, "run.plan_rejected"), 1, "{seen:?}");
    assert_eq!(count(&events, "task.created"), 2, "the revised plan runs");
    assert_eq!(harness.roles.plan_calls(), 2);
    let contexts = harness.roles.plan_contexts();
    assert!(
        contexts[1].feedback.is_some(),
        "the re-plan call carries the rejection"
    );
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 6 / 7 — invalid plans
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_6_plan_invalid_cycle_repaired() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("refactor"))
            .with_plan(plan_with_cycle())
            .with_plan(plan_of(1)),
    );
    let harness = Harness::boot(Setup::new().roles(roles)).await;
    let run = harness.start("refactor", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(harness.roles.plan_calls(), 2, "one repair call");
    let contexts = harness.roles.plan_contexts();
    assert!(
        !contexts[1].repair_errors.is_empty(),
        "the repair call carries the validation errors"
    );
    assert_eq!(count(&events, "run.plan_proposed"), 1, "{seen:?}");
    assert_eq!(count(&events, "run.completed"), 1, "{seen:?}");
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_7_plan_invalid_twice_fails_the_run() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("refactor"))
            .with_plan(plan_with_cycle()),
    );
    let harness = Harness::boot(Setup::new().roles(roles)).await;
    let run = harness.start("refactor", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(harness.roles.plan_calls(), 2, "the plan is repaired once");
    assert_eq!(count(&events, "run.plan_proposed"), 0, "{seen:?}");
    assert_eq!(failure_reason(&events), "invalid_plan");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 8 — the scheduler respects budget.max_parallel
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_8_dag_parallelism_respected() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("four things"))
            .with_plan(plan_of(4)),
    );
    let scenario = Scenario::replying("done").with_default(Rule::replying("done").delay_ms(120));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let budget = kevin_domain::Budget {
        max_parallel: 2,
        ..default_budget()
    };
    let run = harness
        .start_with("four things", RunMode::Headless, budget)
        .await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(count(&events, "task.attempt_started"), 4, "{seen:?}");
    assert_eq!(count(&events, "task.attempt_succeeded"), 4, "{seen:?}");
    assert!(
        peak_concurrency(&events) <= 2,
        "never more than budget.max_parallel attempts in flight: {seen:?}"
    );
    assert!(
        peak_concurrency(&events) >= 2,
        "the scheduler must actually fan out: {seen:?}"
    );
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 9 — a permanent failure skips its dependents and fails the run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_9_dependency_skip() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("two chained things"))
            .with_plan(plan_chain()),
    );
    let scenario = Scenario::replying("done")
        .rule(Rule::matching("first task").fail(FailureClass::Permanent, "invalid spec"));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let run = harness.start("two chained things", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &["task.attempt_failed", "task.skipped", "run.failed"],
    );
    assert_eq!(count(&events, "task.skipped"), 1, "{seen:?}");
    let skipped = events
        .iter()
        .find(|e| e.envelope.event_type == "task.skipped")
        .expect("task.skipped");
    assert_eq!(skipped.envelope.payload["reason"], "dependency_failed");
    assert_eq!(count(&events, "task.retried"), 0, "permanent: no retry");
    assert_eq!(failure_reason(&events), "task_failed");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 10 / 11 — retries
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_10_transient_retry_reroutes() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("flaky"))
            .with_plan(plan_of(1)),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let inner: Arc<dyn Worker> = Arc::new(FakeWorker::new(Scenario::replying("done"), dir.path()));
    let worker: Arc<dyn Worker> = Arc::new(FlakyWorker::new(inner, 1, FailureClass::Transient));
    let harness = Harness::boot(Setup::new().roles(roles).worker(worker)).await;
    let run = harness.start("flaky", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &[
            "task.attempt_failed",
            "task.retried",
            "task.routed",
            "task.attempt_started",
            "task.attempt_succeeded",
            "run.completed",
        ],
    );
    let failed = events
        .iter()
        .find(|e| e.envelope.event_type == "task.attempt_failed")
        .expect("task.attempt_failed");
    assert_eq!(failed.envelope.payload["class"], "transient");
    assert!(failed.envelope.payload["retry_possible"].as_bool().unwrap());
    let aliases = routed_aliases(&events);
    assert_eq!(aliases.len(), 2, "the retry re-routes: {aliases:?}");
    assert_ne!(aliases[0], aliases[1], "the failed alias is excluded");
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_11_max_attempts_exhausted() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("always fails"))
            .with_plan(plan_of(1)),
    );
    let scenario = Scenario::replying("done")
        .with_default(Rule::default().fail(FailureClass::Transient, "simulated 429"));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let run = harness.start("always fails", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(
        count(&events, "task.attempt_failed"),
        2,
        "budget.max_attempts = 2: {seen:?}"
    );
    assert_eq!(count(&events, "task.retried"), 1, "{seen:?}");
    assert_eq!(failure_reason(&events), "task_failed");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 12 — budget exhaustion mid-run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_12_budget_exhausted_mid_run() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("expensive"))
            .with_plan(plan_of(1)),
    );
    let expensive = WorkerUsage {
        cost_usd: Some(rust_decimal::Decimal::new(500, 2)),
        ..WorkerUsage::tokens(1_000, 500)
    };
    let scenario = Scenario::replying("done").with_default(
        Rule::replying("done")
            .event(ScriptedEvent::Usage(expensive))
            .delay_ms(60),
    );
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let budget = kevin_domain::Budget {
        max_usd: Some(rust_decimal::Decimal::new(1, 0)),
        ..default_budget()
    };
    let run = harness
        .start_with("expensive", RunMode::Headless, budget)
        .await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &[
            "run.usage_recorded",
            "run.budget_exhausted",
            "task.attempt_failed",
            "run.failed",
        ],
    );
    let exhausted = events
        .iter()
        .find(|e| e.envelope.event_type == "run.budget_exhausted")
        .expect("run.budget_exhausted");
    assert_eq!(exhausted.envelope.payload["dimension"], "usd");
    let failed = events
        .iter()
        .find(|e| e.envelope.event_type == "task.attempt_failed")
        .expect("task.attempt_failed");
    assert_eq!(failed.envelope.payload["class"], "budget");
    assert_eq!(failure_reason(&events), "budget_exhausted");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 13 — a worker question becomes a Question
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_13_task_input_request() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("needs input"))
            .with_plan(plan_of(1)),
    );
    let scenario = Scenario::replying("done").with_default(Rule::replying("done").event(
        ScriptedEvent::InputRequested {
            question: "Overwrite the config?".to_owned(),
            options: vec!["yes".to_owned(), "no".to_owned()],
        },
    ));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let run = harness.start("needs input", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_order(
        &seen,
        &[
            "task.attempt_started",
            "question.asked",
            "task.input_requested",
            "question.answered",
            "task.input_provided",
            "task.attempt_succeeded",
            "run.completed",
        ],
    );
    let asked = events
        .iter()
        .find(|e| e.envelope.event_type == "question.asked")
        .expect("question.asked");
    assert!(
        asked.envelope.payload["task_id"].is_string(),
        "a worker question belongs to its task"
    );
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 14 — cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_14_cancel_run_kills_children() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("long running"))
            .with_plan(plan_of(2)),
    );
    let scenario = Scenario::replying("done").with_default(Rule::default().hold());
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let run = harness.start("long running", RunMode::Headless).await;

    harness.wait_for_n(run, "task.attempt_started", 2).await;
    harness.cancel(run, "operator asked").await;
    let events = harness
        .wait_until(run, "both attempts cancelled", |events| {
            count(events, "task.attempt_failed") >= 2
        })
        .await;
    let seen = types(&events);

    assert_eq!(count(&events, "run.cancelled"), 1, "{seen:?}");
    for event in events
        .iter()
        .filter(|e| e.envelope.event_type == "task.attempt_failed")
    {
        assert_eq!(event.envelope.payload["class"], "cancelled");
    }
    assert_eq!(count(&events, "task.cancelled"), 2, "{seen:?}");
    assert_eq!(count(&events, "task.retried"), 0, "cancelled: no retry");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 15 — restart terminalises running attempts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_15_runtime_restarted_on_boot() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("survives a restart"))
            .with_plan(plan_of(1)),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let inner: Arc<dyn Worker> = Arc::new(FakeWorker::new(
        Scenario::replying("done").with_default(Rule::default().hold()),
        dir.path(),
    ));
    let holding = Arc::new(HoldOnce::new(inner));
    let mut harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .worker(holding.clone() as Arc<dyn Worker>),
    )
    .await;
    let run = harness.start("survives a restart", RunMode::Headless).await;
    harness.wait_for(run, "task.attempt_started").await;
    common::eventually("the held attempt to reach the worker", || {
        holding.started() == 1
    })
    .await;

    harness.crash().await;
    harness.reboot().await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);
    let restarted = events
        .iter()
        .find(|e| e.envelope.event_type == "task.attempt_failed")
        .expect("task.attempt_failed");
    assert_eq!(restarted.envelope.payload["class"], "runtime_restarted");
    assert_eq!(restarted.envelope.payload["message"], "runtime_restarted");
    assert_order(
        &seen,
        &[
            "task.attempt_started",
            "task.attempt_failed",
            "task.retried",
            "task.attempt_started",
            "task.attempt_succeeded",
            "run.completed",
        ],
    );
    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_15b_kohral_runtime_restarted_fails_the_turn() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("kohral turn"))
            .with_plan(plan_of(1)),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let inner: Arc<dyn Worker> = Arc::new(FakeWorker::new(
        Scenario::replying("done").with_default(Rule::default().hold()),
        dir.path(),
    ));
    let holding = Arc::new(HoldOnce::new(inner));
    let mut harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .worker(holding.clone() as Arc<dyn Worker>),
    )
    .await;
    let run = harness
        .start(
            "kohral turn",
            RunMode::Kohral {
                turn_id: "turn-1".to_owned(),
                session_key: "session".to_owned(),
                session_id: "sid".to_owned(),
            },
        )
        .await;
    harness.wait_for(run, "task.attempt_started").await;
    common::eventually("the held attempt to reach the worker", || {
        holding.started() == 1
    })
    .await;

    harness.crash().await;
    harness.reboot().await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);
    assert_eq!(
        count(&events, "task.retried"),
        0,
        "Kohral never retries a restart: {seen:?}"
    );
    assert_eq!(failure_reason(&events), "runtime_restarted");
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 16 — drain and shutdown
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_16_shutdown_drain() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("two slow things"))
            .with_plan(plan_of(2)),
    );
    let scenario = Scenario::replying("done").with_default(Rule::replying("done").delay_ms(3_000));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let budget = kevin_domain::Budget {
        max_parallel: 1,
        ..default_budget()
    };
    let run = harness
        .start_with("two slow things", RunMode::Headless, budget)
        .await;
    harness.wait_for(run, "task.attempt_started").await;

    harness.handle.drain().await;
    assert!(!harness.handle.is_admitting(), "drain stops admission");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let during_drain = harness.events(run).await;
    assert_eq!(
        count(&during_drain, "task.attempt_started"),
        1,
        "draining schedules no new attempt: {:?}",
        types(&during_drain)
    );

    harness.handle.shutdown().await;
    let events = harness.events(run).await;
    let seen = types(&events);
    let failed = events
        .iter()
        .find(|e| e.envelope.event_type == "task.attempt_failed")
        .unwrap_or_else(|| panic!("task.attempt_failed in {seen:?}"));
    assert_eq!(failed.envelope.payload["message"], "runtime_shutdown");
    assert_eq!(failed.envelope.payload["class"], "transient");
    assert_eq!(count(&events, "run.completed"), 0, "{seen:?}");
}

// ---------------------------------------------------------------------------
// 17 — integration conflicts spawn an Integrate task
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_17_integration_conflict_task() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("two branches"))
            .with_plan(plan_of(2))
            .with_summary("conflicts resolved"),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let workspaces = Arc::new(
        TempWorkspaces::new(dir.path())
            .with_conflicts(&["kevin/t1"])
            .with_integration(kevin_orchestrator::ports::IntegrationOutcome::default()),
    );
    let harness = Harness::boot(Setup::new().roles(roles).workspaces(workspaces)).await;
    let run = harness.start("two branches", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(
        count(&events, "task.created"),
        3,
        "two plan tasks plus the conflict-resolution task: {seen:?}"
    );
    let integrate = events
        .iter()
        .filter(|e| e.envelope.event_type == "task.created")
        .find(|e| e.envelope.payload["kind"] == "integrate")
        .unwrap_or_else(|| panic!("a task.created with kind `integrate` in {seen:?}"));
    assert!(
        integrate.envelope.payload["spec"]["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("kevin/t1")),
        "the conflict list reaches the task"
    );
    assert_eq!(count(&events, "run.integrated"), 1, "{seen:?}");
    assert_eq!(count(&events, "run.completed"), 1, "{seen:?}");
    assert_eq!(
        harness.workspaces.integrations(),
        2,
        "one retry after the fix"
    );
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 18 — a hanging judge never blocks the run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_18_evaluation_timeout_completes_run() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("evaluate me"))
            .with_plan(plan_of(1)),
    );
    let evaluator = Arc::new(ScriptedEvaluator::slow(Duration::from_secs(30)));
    let memory = Arc::new(RecordingMemory::with_context("- prefer small diffs"));
    let harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .evaluator(evaluator.clone())
            .memory(memory.clone())
            .config(|config| {
                config.orchestrator.evaluation_timeout = Duration::from_millis(120);
            }),
    )
    .await;
    let run = harness.start("evaluate me", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    assert_eq!(count(&events, "run.evaluated"), 0, "{seen:?}");
    let completed = events
        .iter()
        .find(|e| e.envelope.event_type == "run.completed")
        .expect("run.completed");
    assert_eq!(completed.envelope.payload["evaluation_skipped"], true);
    assert_eq!(evaluator.calls(), 1);
    assert_eq!(
        harness.roles.plan_contexts()[0].memory.as_deref(),
        Some("- prefer small diffs")
    );
    common::eventually("the run summary to be remembered", || {
        memory.lessons().len() == 1
    })
    .await;
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// 19 / 20 — command handling
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_19_idempotent_command_replay() {
    kevin_testkit::skip_unless_pg!();
    let services = Services::new().await;
    let run_id = services.ids.run_id();
    let ctx = services.ctx(run_id);
    let cmd = services.start_run(run_id, "do the thing", RunMode::Headless);

    let first = services
        .runs
        .start(cmd.clone(), &ctx)
        .await
        .expect("first start");
    let second = services
        .runs
        .start(cmd, &ctx)
        .await
        .expect("replayed start");

    assert_eq!(first, second, "the replay returns the original result");
    let events = services.events(run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| e.envelope.event_type == "run.started")
            .count(),
        1,
        "the replay appends nothing"
    );
    let run = services.runs.load(run_id).await.expect("load run");
    assert_eq!(kevin_domain::Aggregate::version(&run), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws08_20_occ_conflict_retry() {
    kevin_testkit::skip_unless_pg!();
    let services = Services::new().await;
    let run_id = services.ids.run_id();
    services
        .runs
        .start(
            services.start_run(run_id, "clarify", RunMode::Interactive),
            &services.ctx(run_id),
        )
        .await
        .expect("start");
    services
        .runs
        .start_understanding(
            run_id,
            StartUnderstanding {
                planner_route: Route::new(
                    WorkerKind::Fake,
                    ModelAlias::new("fake").expect("alias"),
                ),
            },
            &services.ctx(run_id),
        )
        .await
        .expect("start understanding");
    let first = services.ids.question_id();
    let second = services.ids.question_id();
    services
        .runs
        .record_understanding(
            run_id,
            RecordUnderstanding {
                understanding: understanding("clarify"),
                usage: Usage::ZERO,
                question_ids: vec![first, second],
            },
            &services.ctx(run_id),
        )
        .await
        .expect("record understanding");

    // Two commands racing on the same stream: both must land.
    let runs = &services.runs;
    let ctx_first = services.ctx(run_id);
    let ctx_second = services.ctx(run_id);
    let (a, b) = tokio::join!(
        runs.note_question_answered(
            run_id,
            kevin_domain::run::NoteQuestionAnswered { question_id: first },
            &ctx_first,
        ),
        runs.note_question_answered(
            run_id,
            kevin_domain::run::NoteQuestionAnswered {
                question_id: second
            },
            &ctx_second,
        ),
    );
    a.expect("first answer applied");
    b.expect("second answer applied");

    let events = services.events(run_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| e.envelope.event_type == "run.question_answered")
            .count(),
        2,
        "no lost update"
    );
    let run = services.runs.load(run_id).await.expect("load run");
    assert!(run.open_question_ids().is_empty());
    assert_eq!(run.status(), kevin_domain::RunStatus::Planning);
}
