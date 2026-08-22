//! WS-25 hardening — cost-cap fuzzing (`plan/12` §WS-25, `plan/09` T7).
//!
//! The property under test is the one an operator cares about: **a run cannot
//! spend arbitrarily more than its budget**. It can overshoot — usage is only
//! known once a worker reports it, so the attempts already in flight when the
//! limit is crossed still cost money — but the overshoot must be bounded by
//! those in-flight attempts and nothing else.
//!
//! This drives the real pieces: [`kevin_domain::Run`] for the budget
//! arithmetic and the `run.budget_exhausted` decision, and
//! [`kevin_orchestrator::run_actor::budget_spent`] — the actual admission gate
//! of `RunActor::schedule` — for the scheduling decision. Only the *timing* is
//! simulated, which is what makes it deterministic: no Postgres, no tokio, no
//! sleeps.
//!
//! Random task graphs (a DAG, so completion order varies), random per-attempt
//! usage including zero-usage and huge-usage attempts, random budgets on each
//! of the three dimensions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kevin_domain::run::{
    ExhaustBudget, NoteTaskTerminal, RunCommand, RunEvent, StartRun, TaskOutcome,
};
use kevin_domain::{Aggregate, Budget, Goal, Plan, PlanTask, Run, RunMode, TaskId, Usage};
use kevin_orchestrator::run_actor::budget_spent;
use proptest::prelude::*;
use rust_decimal::Decimal;

/// One generated task: its dependencies (indices into the plan, always
/// smaller, so the graph is acyclic) and what the attempt will cost.
#[derive(Debug, Clone)]
struct GenTask {
    depends_on: Vec<usize>,
    cost_cents: i64,
    input_tokens: u64,
    output_tokens: u64,
}

fn gen_task(index: usize) -> impl Strategy<Value = GenTask> {
    (
        proptest::collection::vec(0..index.max(1), 0..3usize),
        // Includes 0 (a worker that reports nothing) and values large enough
        // to blow any budget in one go.
        prop_oneof![Just(0i64), 1i64..500, 500i64..20_000],
        prop_oneof![Just(0u64), 1u64..5_000],
        prop_oneof![Just(0u64), 1u64..5_000],
    )
        .prop_map(
            move |(deps, cost_cents, input_tokens, output_tokens)| GenTask {
                depends_on: if index == 0 {
                    Vec::new()
                } else {
                    deps.into_iter().filter(|d| *d < index).collect()
                },
                cost_cents,
                input_tokens,
                output_tokens,
            },
        )
}

fn gen_plan() -> impl Strategy<Value = Vec<GenTask>> {
    (1usize..12).prop_flat_map(|n| {
        let tasks: Vec<_> = (0..n).map(gen_task).collect();
        tasks
    })
}

/// A budget that constrains at least one dimension often enough to matter.
fn gen_budget() -> impl Strategy<Value = Budget> {
    (
        prop_oneof![Just(None), (1i64..30_000).prop_map(Some)],
        prop_oneof![Just(None), (1u64..40_000).prop_map(Some)],
        1u16..6,
    )
        .prop_map(|(usd_cents, tokens, max_parallel)| Budget {
            max_usd: usd_cents.map(|c| Decimal::new(c, 2)),
            max_tokens: tokens,
            max_wall: Some(Duration::from_secs(3600)),
            max_attempts: 1,
            max_parallel,
        })
}

/// A `Run` in `executing` with `count` plan tasks.
fn executing_run(budget: Budget, task_ids: &[TaskId]) -> Run {
    let mut run = Run::default();
    let cmd = StartRun {
        run_id: kevin_domain::RunId::new(),
        goal: Goal::new("fuzz the budget", "/tmp"),
        mode: RunMode::Headless,
        budget,
        requested_by: "proptest".to_owned(),
        auto_approve_plans: true,
    };
    let started = run.handle(&RunCommand::Start(cmd)).expect("start");
    apply_all(&mut run, &started);
    // Fast-forward to `executing` by applying the events of the phases this
    // property does not exercise; the budget arithmetic is unaffected.
    let plan = Plan::new(
        (0..task_ids.len())
            .map(|i| PlanTask::new(format!("t{i}"), "implement", format!("task {i}")))
            .collect(),
        "fuzz",
    );
    apply_all(
        &mut run,
        &[
            RunEvent::PlanProposed {
                plan,
                usage: Usage::ZERO,
                revision: 0,
            },
            RunEvent::PlanApproved {
                by: "auto".to_owned(),
            },
            RunEvent::ExecutionStarted {
                task_ids: task_ids.to_vec(),
            },
        ],
    );
    run
}

fn apply_all(run: &mut Run, events: &[RunEvent]) {
    for event in events {
        run.apply(event);
    }
}

/// The single-attempt usage of `task`.
fn usage_of(task: &GenTask) -> Usage {
    Usage {
        cost_usd: Some(Decimal::new(task.cost_cents, 2)),
        input_tokens: task.input_tokens,
        output_tokens: task.output_tokens,
        ..Usage::ZERO
    }
}

/// What the simulation observed.
struct Outcome {
    /// Usage the run ended up holding.
    final_usage: Usage,
    /// Usage of the attempts that were in flight when admission stopped.
    inflight_when_stopped: Vec<Usage>,
    /// How many `run.budget_exhausted` events the aggregate produced.
    exhausted_events: usize,
    /// Tasks that were never admitted.
    never_admitted: usize,
}

/// Runs the real admission gate and the real aggregate over `tasks`.
///
/// Each round admits every ready task the gate allows (up to `max_parallel`),
/// then completes **one** of the in-flight attempts — the worst case for the
/// overshoot bound, because the other in-flight attempts keep spending.
fn simulate(budget: Budget, tasks: &[GenTask]) -> Outcome {
    let ids: Vec<TaskId> = tasks.iter().map(|_| TaskId::new()).collect();
    let mut run = executing_run(budget.clone(), &ids);
    let max_parallel = usize::from(budget.max_parallel.max(1));

    let mut done: BTreeSet<usize> = BTreeSet::new();
    let mut inflight: Vec<usize> = Vec::new();
    let mut exhausted_events = 0usize;
    let mut inflight_when_stopped: Vec<Usage> = Vec::new();
    let mut stopped = false;
    let mut usage_by_task: BTreeMap<TaskId, Usage> = BTreeMap::new();

    loop {
        // --- admission ------------------------------------------------------
        // The gate is consulted *before* dispatching, exactly as
        // `RunActor::schedule` does. Nothing is admitted once it is closed.
        if !budget_spent(&run) {
            for (index, task) in tasks.iter().enumerate() {
                if inflight.len() >= max_parallel {
                    break;
                }
                if done.contains(&index) || inflight.contains(&index) {
                    continue;
                }
                if task.depends_on.iter().all(|d| done.contains(d)) {
                    inflight.push(index);
                }
            }
        }

        let Some(finished) = inflight.first().copied() else {
            break;
        };
        inflight.remove(0);
        done.insert(finished);

        // --- the attempt reports its usage ----------------------------------
        let usage = usage_of(&tasks[finished]);
        usage_by_task.insert(ids[finished], usage);
        let events = run
            .handle(&RunCommand::NoteTaskTerminal(NoteTaskTerminal {
                task_id: ids[finished],
                outcome: TaskOutcome::Succeeded,
                usage,
            }))
            .expect("noting a terminal task is always legal while executing");
        exhausted_events += events
            .iter()
            .filter(|e| matches!(e, RunEvent::BudgetExhausted { .. }))
            .count();
        apply_all(&mut run, &events);

        // The overshoot is whatever was *already dispatched* when the limit
        // was crossed: the attempts still running plus the one whose report
        // crossed it. Everything dispatched later would break the bound, and
        // the gate above is what stops that from happening.
        if !stopped && budget_spent(&run) {
            stopped = true;
            inflight_when_stopped = inflight
                .iter()
                .map(|i| usage_of(&tasks[*i]))
                .chain(std::iter::once(usage))
                .collect();
        }
    }

    Outcome {
        final_usage: *run.usage(),
        inflight_when_stopped,
        exhausted_events,
        never_admitted: tasks.len() - done.len(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// The overshoot is bounded by the attempts that were already in flight.
    ///
    /// Concretely: `spent ≤ limit + Σ(usage of the attempts in flight when the
    /// gate closed)`. Any admission that happens *after* the gate closes breaks
    /// this, which is exactly the regression the pre-dispatch gate prevents.
    #[test]
    fn ac_ws25_7_1_a_run_never_overshoots_its_budget_beyond_the_in_flight_attempts(
        budget in gen_budget(),
        tasks in gen_plan(),
    ) {
        let outcome = simulate(budget.clone(), &tasks);

        if let Some(limit) = budget.max_usd {
            let spent = outcome.final_usage.cost_usd.unwrap_or_default();
            let inflight: Decimal = outcome
                .inflight_when_stopped
                .iter()
                .filter_map(|u| u.cost_usd)
                .sum();
            prop_assert!(
                spent <= limit + inflight,
                "spent {spent} > limit {limit} + in-flight {inflight}",
            );
        }
        if let Some(limit) = budget.max_tokens {
            let spent = outcome.final_usage.total_tokens();
            let inflight: u64 = outcome
                .inflight_when_stopped
                .iter()
                .map(Usage::total_tokens)
                .sum();
            prop_assert!(
                spent <= limit + inflight,
                "spent {spent} tokens > limit {limit} + in-flight {inflight}",
            );
        }
    }

    /// `run.budget_exhausted` is emitted at most once, and exactly when the
    /// recorded usage crossed a limit. A second one would fail the run twice.
    #[test]
    fn ac_ws25_7_2_budget_exhausted_is_emitted_once_and_only_when_it_is_true(
        budget in gen_budget(),
        tasks in gen_plan(),
    ) {
        let outcome = simulate(budget.clone(), &tasks);
        prop_assert!(
            outcome.exhausted_events <= 1,
            "{} budget_exhausted events",
            outcome.exhausted_events,
        );
        let crossed = budget.exceeded_by(&outcome.final_usage).is_some();
        prop_assert_eq!(
            outcome.exhausted_events == 1,
            crossed,
            "exhausted={} but crossed={} for usage {:?} against {:?}",
            outcome.exhausted_events,
            crossed,
            outcome.final_usage,
            budget,
        );
    }

    /// Once the gate closes, no further task is admitted: any task left over
    /// stayed pending. An unbounded budget, conversely, always runs everything.
    #[test]
    fn ac_ws25_7_3_admission_stops_at_the_gate_and_never_stalls_an_affordable_run(
        budget in gen_budget(),
        tasks in gen_plan(),
    ) {
        let outcome = simulate(budget.clone(), &tasks);
        if outcome.never_admitted > 0 {
            prop_assert!(
                budget.exceeded_by(&outcome.final_usage).is_some(),
                "{} tasks were skipped while the run was still within budget",
                outcome.never_admitted,
            );
        }

        // The counter-check: with no cost limits nothing is ever skipped, so
        // the gate cannot be stalling runs that could afford to continue.
        let unlimited = Budget {
            max_usd: None,
            max_tokens: None,
            ..budget
        };
        let free = simulate(unlimited, &tasks);
        prop_assert_eq!(free.never_admitted, 0);
        prop_assert_eq!(free.exhausted_events, 0);
    }
}

/// The wall-clock dimension is not usage-driven — the actor observes it on its
/// tick — so it gets its own check: crossing it is reported exactly once, and
/// the second report is refused by the aggregate rather than duplicated.
#[test]
fn ac_ws25_7_4_a_wall_clock_budget_is_exhausted_exactly_once() {
    let budget = Budget {
        max_usd: None,
        max_tokens: None,
        max_wall: Some(Duration::from_secs(60)),
        max_attempts: 1,
        max_parallel: 1,
    };
    let ids = [TaskId::new()];
    let mut run = executing_run(budget.clone(), &ids);

    let excess = budget
        .wall_exceeded_by(Duration::from_secs(90))
        .expect("90s exceeds a 60s wall budget");
    let events = run
        .handle(&RunCommand::ExhaustBudget(ExhaustBudget { excess }))
        .expect("first exhaustion");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], RunEvent::BudgetExhausted { .. }));
    apply_all(&mut run, &events);

    assert!(
        budget_spent(&run),
        "the gate must close on a wall-clock stop"
    );
    // A second report is refused rather than recorded twice: the actor treats
    // an invalid transition as "already done" (`err.is_invalid_transition()`),
    // so a tick that fires again cannot produce a duplicate event.
    let again = run.handle(&RunCommand::ExhaustBudget(ExhaustBudget { excess }));
    assert!(
        again.is_err(),
        "the wall-clock budget was exhausted twice: {again:?}"
    );
    // Under the wall budget nothing is reported at all.
    assert!(budget.wall_exceeded_by(Duration::from_secs(30)).is_none());
}
