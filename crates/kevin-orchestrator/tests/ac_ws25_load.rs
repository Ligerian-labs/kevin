//! WS-25 hardening — the load scenario (`plan/12` §WS-25: "load test with 50
//! fake tasks").
//!
//! Gated behind `KEVIN_LOAD_TESTS=1` so `just ci` stays fast; the gate is an
//! environment variable rather than a cargo feature because CI decides per
//! *job*, not per build, and a feature would silently drop the test from
//! `--all-features` runs that do want it.
//!
//! ```sh
//! KEVIN_LOAD_TESTS=1 cargo nextest run -p kevin-orchestrator --test ac_ws25_load
//! ```
//!
//! What it pins down, none of which the 1–4 task scenarios can:
//!
//! - the concurrency bulkheads hold at scale: never more than
//!   `budget.max_parallel` attempts in flight, over 50 tasks and ~350 events;
//! - the read models are not permanently behind after the burst — the
//!   projection checkpoint reaches the store head;
//! - memory stays bounded: the actor holds state per *running* attempt, not
//!   per task, so 50 tasks must not cost 50 tasks' worth of resident memory;
//! - the wall-clock, printed for the record.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Harness, Setup, plan_of, understanding};
use kevin_bus::EventBus;
use kevin_domain::{Budget, RunMode};
use kevin_orchestrator::testing::ScriptedRoles;
use kevin_store::{EventStore, StoredEvent};
use kevin_worker::fake::{Rule, Scenario};
use tokio_util::sync::CancellationToken;

/// How many plan tasks the run carries.
const TASKS: u32 = 50;
/// The bulkhead under test.
const MAX_PARALLEL: u16 = 8;
/// Generous: a fake attempt takes ~30 ms, so 50 of them at 8-wide is seconds.
const DEADLINE: Duration = Duration::from_secs(300);

/// Highest number of attempts in flight at any point of the stream.
fn peak_concurrency(events: &[StoredEvent]) -> usize {
    let mut current = 0usize;
    let mut peak = 0usize;
    for event in events {
        match event.envelope.event_type {
            "task.attempt_started" => {
                current += 1;
                peak = peak.max(current);
            }
            "task.attempt_succeeded" | "task.attempt_failed" => current = current.saturating_sub(1),
            _ => {}
        }
    }
    peak
}

fn count(events: &[StoredEvent], event_type: &str) -> usize {
    events
        .iter()
        .filter(|e| e.envelope.event_type == event_type)
        .count()
}

/// Resident set size of this process in KiB, when the platform can tell us.
fn rss_kib() -> Option<u64> {
    #[cfg(unix)]
    {
        let pid = std::process::id();
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_10_1_fifty_fake_tasks_respect_the_bulkheads_and_stay_bounded() {
    if std::env::var("KEVIN_LOAD_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set KEVIN_LOAD_TESTS=1 to run the load scenario");
        return;
    }
    kevin_testkit::skip_unless_pg!();

    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("fifty small things"))
            .with_plan(plan_of(TASKS as usize)),
    );
    // Each attempt is slower than the saga tick, so attempts genuinely pile up
    // and the semaphore is exercised instead of the run trickling one task per
    // tick and never testing anything.
    let scenario = Scenario::replying("done").with_default(Rule::replying("done").delay_ms(300));
    let harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .scenario(scenario)
            .config(|config| {
                // The per-kind bulkhead must not be the binding constraint:
                // this asserts the *global* `budget.max_parallel`.
                config
                    .concurrency
                    .per_worker_kind
                    .insert(kevin_domain::WorkerKind::Fake, 64);
                config.budget.max_parallel_tasks = 64;
                // The default plan cap is 24 tasks; this scenario is about
                // what happens past it.
                config.orchestrator.max_tasks_per_run = TASKS;
            }),
    )
    .await;

    // The WS-08 harness boots the engine but no projection followers (its
    // scenarios assert on `core.events`). Projection lag is part of what this
    // scenario measures, so they are started here, over the same bus.
    let projections_cancel = CancellationToken::new();
    let projections = kevin_orchestrator::projections::spawn_all(
        harness.db.pool(),
        &(Arc::clone(&harness.store) as Arc<dyn EventStore>),
        &(Arc::clone(&harness.bus) as Arc<dyn EventBus>),
        &projections_cancel,
    );

    let baseline_rss = rss_kib();
    let started = Instant::now();
    let run = harness
        .start_with(
            "fifty small things",
            RunMode::Headless,
            Budget {
                max_parallel: MAX_PARALLEL,
                max_usd: None,
                max_tokens: None,
                max_wall: Some(Duration::from_secs(600)),
                max_attempts: 1,
            },
        )
        .await;

    // The harness' own `wait_terminal` deadline is sized for 1–4 tasks.
    let deadline = Instant::now() + DEADLINE;
    let events = loop {
        let events = harness.events(run).await;
        if events.iter().any(|e| {
            matches!(
                e.envelope.event_type,
                "run.completed" | "run.failed" | "run.cancelled"
            )
        }) {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not finish in {DEADLINE:?}; {} attempts started, {} finished",
            count(&events, "task.attempt_started"),
            count(&events, "task.attempt_succeeded") + count(&events, "task.attempt_failed"),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let wall = started.elapsed();

    // -- the run actually did the work ---------------------------------------
    assert_eq!(
        count(&events, "task.created"),
        TASKS as usize,
        "the plan did not produce {TASKS} tasks"
    );
    assert_eq!(
        count(&events, "task.attempt_succeeded"),
        TASKS as usize,
        "not every task succeeded"
    );
    assert_eq!(count(&events, "run.completed"), 1);

    // -- the bulkhead held ----------------------------------------------------
    let peak = peak_concurrency(&events);
    assert!(
        peak <= usize::from(MAX_PARALLEL),
        "peak concurrency {peak} exceeded max_parallel {MAX_PARALLEL}"
    );
    assert!(
        peak >= usize::from(MAX_PARALLEL) / 2,
        "peak concurrency was only {peak} of {MAX_PARALLEL}: the run effectively \
         serialised, so the bulkhead was never exercised"
    );

    // -- the read models caught up -------------------------------------------
    let head = harness.store.head_position().await.expect("head");
    let checkpoints = kevin_store::Checkpoints::new(harness.db.pool().clone());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let at = checkpoints
            .get("task_board")
            .await
            .expect("checkpoint")
            .unwrap_or(0);
        if at >= head {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the task_board projection stayed {} events behind the head",
            head - at
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM orch.task_board WHERE run_id = $1")
        .bind(run.as_uuid())
        .fetch_one(harness.db.pool())
        .await
        .expect("count task_board");
    assert_eq!(rows, i64::from(TASKS), "the board is missing tasks");

    // -- memory stayed bounded ------------------------------------------------
    // The engine keeps per-*attempt* state, so 50 tasks at 8-wide must not
    // grow the process by more than a fixed slab. The bound is deliberately
    // loose (allocator behaviour and Postgres pools vary); it catches a leak
    // of the "one buffer per task, never freed" kind, which is the failure
    // mode worth guarding.
    if let (Some(before), Some(after)) = (baseline_rss, rss_kib()) {
        let growth_mib = after.saturating_sub(before) / 1024;
        eprintln!("ac_ws25_10_1: RSS {before} KiB -> {after} KiB (+{growth_mib} MiB)");
        assert!(
            growth_mib < 512,
            "the process grew by {growth_mib} MiB running {TASKS} fake tasks"
        );
    }

    eprintln!(
        "ac_ws25_10_1: {TASKS} tasks, max_parallel {MAX_PARALLEL}, peak {peak}, \
         {} events, wall {:.2}s",
        events.len(),
        wall.as_secs_f64(),
    );
    projections_cancel.cancel();
    for handle in projections {
        let _ = handle.await;
    }
    harness.shutdown().await;
}
