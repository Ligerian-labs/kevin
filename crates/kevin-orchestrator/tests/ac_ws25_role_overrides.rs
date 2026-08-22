//! WS-25 follow-up — per-run role overrides (`StartRun.role_overrides`).
//!
//! WS-22 could validate a Kohral turn's `model` field and record it on the
//! ledger, but not *apply* it: `StartRun` had no per-run role field, and
//! mutating `[roles]` in a daemon that serves other runs concurrently is not an
//! option. The plan change (`plan/02` §Run, `plan/05` §3.1) adds
//! `role_overrides`, and these are its acceptance tests.
//!
//! Needs Postgres; skips cleanly without it.

mod common;

use std::sync::Arc;

use common::{Harness, Setup, plan_of, understanding};
use kevin_domain::{ModelAlias, RoleOverrides, RunMode};
use kevin_orchestrator::run_actor::{DEFAULT_ROLE_KEY, role_route};
use kevin_orchestrator::testing::ScriptedRoles;
use kevin_store::StoredEvent;

fn aliases_of(events: &[StoredEvent], event_type: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.envelope.event_type == event_type)
        .filter_map(|e| {
            e.envelope.payload["route"]["model"]
                .as_str()
                .map(ToOwned::to_owned)
        })
        .collect()
}

/// `role_route` prefers the run's override and reports a bad one clearly.
#[test]
fn ac_ws25_12_1_role_route_prefers_the_per_run_override() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let config = common::test_config(tmp.path());
    let planner = kevin_config::Role::Planner;

    // No override: the configured alias.
    let plain = role_route(&config, planner, &RoleOverrides::new()).expect("configured route");
    assert_eq!(plain.model.as_str(), common::ALIAS);

    // With one: the override wins.
    let overrides: RoleOverrides = [(
        "planner".to_owned(),
        ModelAlias::new(common::ALIAS_ALT).expect("alias"),
    )]
    .into_iter()
    .collect();
    let overridden = role_route(&config, planner, &overrides).expect("overridden route");
    assert_eq!(overridden.model.as_str(), common::ALIAS_ALT);

    // An override for a *different* role does not leak.
    let elsewhere: RoleOverrides = [(
        "judge".to_owned(),
        ModelAlias::new(common::ALIAS_ALT).expect("alias"),
    )]
    .into_iter()
    .collect();
    assert_eq!(
        role_route(&config, planner, &elsewhere)
            .expect("route")
            .model
            .as_str(),
        common::ALIAS
    );

    // An alias that is not in `[models]` fails, and the message says it came
    // from the override rather than blaming the configuration.
    let unknown: RoleOverrides = [(
        "planner".to_owned(),
        ModelAlias::new("not-configured").expect("alias"),
    )]
    .into_iter()
    .collect();
    let err = role_route(&config, planner, &unknown).expect_err("unknown alias");
    assert!(err.contains("role_overrides.planner"), "{err}");
    assert!(err.contains("not-configured"), "{err}");
}

/// End to end: a run started with a `default` override routes **every plan
/// task** to that alias instead of letting the router choose.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_12_2_a_default_override_pins_every_task_of_the_run() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("pin the model"))
            .with_plan(plan_of(3)),
    );
    let mut harness = Harness::boot(Setup::new().roles(roles)).await;
    // The router would hand out `fake` (its first candidate); the override
    // asks for the other alias, so the assertion cannot pass by accident.
    harness.role_overrides = [
        (
            DEFAULT_ROLE_KEY.to_owned(),
            ModelAlias::new(common::ALIAS_ALT).expect("alias"),
        ),
        (
            "planner".to_owned(),
            ModelAlias::new(common::ALIAS_ALT).expect("alias"),
        ),
    ]
    .into_iter()
    .collect();

    let run = harness.start("pin the model", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;

    let routed = aliases_of(&events, "task.routed");
    assert_eq!(routed.len(), 3, "three tasks were routed");
    assert!(
        routed.iter().all(|alias| alias == common::ALIAS_ALT),
        "every task must run on the pinned alias, got {routed:?}"
    );
    // The planner phase used it too.
    let planner = events
        .iter()
        .find(|e| e.envelope.event_type == "run.understanding_started")
        .expect("run.understanding_started");
    assert_eq!(
        planner.envelope.payload["planner_route"]["model"],
        common::ALIAS_ALT
    );
    harness.shutdown().await;
}

/// The override is on `run.started`, so a runtime that reboots mid-run resumes
/// on the model the caller asked for rather than falling back to `[roles]`.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_12_3_the_override_survives_a_restart() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("survive with the pin"))
            .with_plan(plan_of(1)),
    );
    let mut harness = Harness::boot(Setup::new().roles(roles)).await;
    harness.role_overrides = [(
        DEFAULT_ROLE_KEY.to_owned(),
        ModelAlias::new(common::ALIAS_ALT).expect("alias"),
    )]
    .into_iter()
    .collect();

    let run = harness
        .start("survive with the pin", RunMode::Headless)
        .await;
    harness.wait_for(run, "run.started").await;

    let started = harness.payload(run, "run.started").await;
    assert_eq!(
        started["role_overrides"][DEFAULT_ROLE_KEY],
        common::ALIAS_ALT,
        "the override must be recorded on the event: {started}"
    );

    harness.crash().await;
    harness.reboot().await;

    let events = harness.wait_terminal(run).await;
    let routed = aliases_of(&events, "task.routed");
    assert!(!routed.is_empty(), "the rebooted run routed its task");
    assert!(
        routed.iter().all(|alias| alias == common::ALIAS_ALT),
        "the rebooted run forgot the override: {routed:?}"
    );
    harness.shutdown().await;
}

/// An override naming an alias that is not in `[models]` fails the run with
/// `no_route` — the same treatment a bad `roles.*` gets, not a silent fallback.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_12_4_an_unknown_override_alias_fails_the_run() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("bad pin"))
            .with_plan(plan_of(1)),
    );
    let mut harness = Harness::boot(Setup::new().roles(roles)).await;
    harness.role_overrides = [(
        "planner".to_owned(),
        ModelAlias::new("no-such-alias").expect("alias"),
    )]
    .into_iter()
    .collect();

    let run = harness.start("bad pin", RunMode::Headless).await;
    let events = harness.wait_terminal(run).await;
    let failed = events
        .iter()
        .find(|e| e.envelope.event_type == "run.failed")
        .expect("run.failed");
    let message = failed.envelope.payload["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("role_overrides.planner") && message.contains("no-such-alias"),
        "the failure must name the override: {message}"
    );
    harness.shutdown().await;
}
