//! Given/when/then coverage of the `Run` state machine
//! (`plan/02-domain-model.md` §Run): every transition, every rejection.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use kevin_domain::aggregate::Aggregate;
use kevin_domain::error::DomainError;
use kevin_domain::kinds::FailureClass;
use kevin_domain::plan::{Plan, PlanError, PlanTask};
use kevin_domain::run::{
    AUTO_APPROVED_BY, MarkEvaluated, NoteTaskTerminal, ProposePlan, RecordTaskUsage, Run,
    RunCommand, RunEvent, RunStatus, StartExecution, StartRun, TaskOutcome,
};
use kevin_domain::values::{Budget, BudgetDimension, RunFailureReason, Usage};
use kevin_testkit::given_when_then::{
    assert_run_invariants, given, given_nothing, ids, run, values,
};

fn usd(s: &str) -> kevin_domain::Decimal {
    s.parse().unwrap()
}

fn status_of(history: &[RunEvent]) -> RunStatus {
    Run::rehydrate(history).status()
}

// ---------------------------------------------------------------------------
// Happy path, transition by transition
// ---------------------------------------------------------------------------

#[test]
fn start_run_creates_received_run() {
    given_nothing::<Run>().when(run::start()).then_state(|r| {
        assert_eq!(r.status(), RunStatus::Received);
        assert_eq!(r.run_id(), ids::run_id());
        assert_eq!(r.requested_by(), "valentin");
        assert_eq!(r.version(), 1);
        assert!(!r.auto_approves_plans());
    });
    given_nothing::<Run>()
        .when(run::start())
        .then(&[run::started()]);
}

#[test]
fn start_run_rejects_existing_run_and_invalid_input() {
    given::<Run>(&[run::started()])
        .when(run::start())
        .then_err(DomainError::AlreadyExists {
            aggregate: "run",
            id: ids::run_id().as_uuid(),
        });
    let mut blank = run::start();
    blank.goal.text = "   ".into();
    given_nothing::<Run>()
        .when(blank)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut zero_attempts = run::start();
    zero_attempts.budget = Budget::unlimited().with_max_attempts(0);
    given_nothing::<Run>()
        .when(zero_attempts)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn received_to_understanding() {
    given::<Run>(&[run::started()])
        .when(run::start_understanding())
        .then(&[run::understanding_started()]);
    assert_eq!(
        status_of(&[run::started(), run::understanding_started()]),
        RunStatus::Understanding
    );
}

#[test]
fn understanding_completed_without_questions_goes_to_planning() {
    given::<Run>(&[run::started(), run::understanding_started()])
        .when(run::record_understanding())
        .then(&[run::understanding_completed()]);
    assert_eq!(status_of(&run::history_planning()), RunStatus::Planning);
}

#[test]
fn understanding_completed_with_questions_awaits_answers_until_last_answer() {
    let q1 = ids::question_id(1);
    let q2 = ids::question_id(2);
    given::<Run>(&[run::started(), run::understanding_started()])
        .when(run::record_understanding_with_questions(vec![q1, q2]))
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::AwaitingAnswers);
            assert_eq!(r.open_question_ids(), &[q1, q2]);
            assert_run_invariants(r);
        });
    let history = vec![
        run::started(),
        run::understanding_started(),
        run::understanding_completed_with_questions(vec![q1, q2]),
    ];
    given::<Run>(&history)
        .when(run::note_question_answered(q1))
        .then(&[run::question_answered(q1, 1)]);
    given::<Run>(&history)
        .when(run::note_question_answered(q1))
        .then_state(|r| assert_eq!(r.status(), RunStatus::AwaitingAnswers));
    let mut one_left = history.clone();
    one_left.push(run::question_answered(q1, 1));
    given::<Run>(&one_left)
        .when(run::note_question_answered(q2))
        .then(&[run::question_answered(q2, 0)]);
    given::<Run>(&one_left)
        .when(run::note_question_answered(q2))
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::Planning);
            assert!(r.open_question_ids().is_empty());
        });
    // Unknown / already answered question.
    given::<Run>(&one_left)
        .when(run::note_question_answered(q1))
        .then_err(DomainError::UnknownQuestion { question_id: q1 });
    given::<Run>(&one_left)
        .when(run::note_question_answered(ids::question_id(9)))
        .then_err(DomainError::UnknownQuestion {
            question_id: ids::question_id(9),
        });
}

#[test]
fn record_understanding_validates_the_understanding() {
    let mut cmd = run::record_understanding();
    cmd.understanding.success_criteria.clear();
    given::<Run>(&[run::started(), run::understanding_started()])
        .when(cmd)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn propose_plan_interactive_awaits_approval() {
    given::<Run>(&run::history_planning())
        .when(run::propose_plan())
        .then(&[run::plan_proposed()]);
    assert_eq!(
        status_of(&run::history_awaiting_approval()),
        RunStatus::AwaitingPlanApproval
    );
}

#[test]
fn propose_plan_auto_approves_in_headless_kohral_and_config_modes() {
    for started in [run::started_headless(), run::started_kohral()] {
        given::<Run>(&[
            started,
            run::understanding_started(),
            run::understanding_completed(),
        ])
        .when(run::propose_plan())
        .then(&[run::plan_proposed(), run::plan_auto_approved()]);
    }
    let mut start = run::start();
    start.auto_approve_plans = true;
    given::<Run>(&[
        run::started_from(&start),
        run::understanding_started(),
        run::understanding_completed(),
    ])
    .when(run::propose_plan())
    .then_state(|r| {
        assert_eq!(r.status(), RunStatus::Executing);
        assert!(r.plan_approved());
        assert!(r.auto_approves_plans());
    });
    assert_eq!(AUTO_APPROVED_BY, "auto");
}

#[test]
fn propose_plan_rejects_invalid_plans() {
    let cyclic = Plan::new(
        vec![
            PlanTask::new("t1", "implement", "a").depends_on(["t2"]),
            PlanTask::new("t2", "test", "b").depends_on(["t1"]),
        ],
        "loop",
    );
    given::<Run>(&run::history_planning())
        .when(ProposePlan::new(cyclic, Usage::ZERO))
        .then_err_matching(|e| {
            matches!(e, DomainError::InvalidPlan(errs) if matches!(errs[..], [PlanError::Cycle { .. }]))
        });
    let big = Plan::new(
        (1..=3)
            .map(|i| PlanTask::new(format!("t{i}"), "implement", "x"))
            .collect(),
        "big",
    );
    given::<Run>(&run::history_planning())
        .when(ProposePlan {
            plan: big,
            usage: Usage::ZERO,
            max_tasks: 2,
        })
        .then_err(DomainError::InvalidPlan(vec![PlanError::TooManyTasks {
            count: 3,
            max: 2,
        }]));
}

#[test]
fn approve_plan_enters_executing() {
    given::<Run>(&run::history_awaiting_approval())
        .when(run::approve_plan())
        .then(&[run::plan_approved()]);
    given::<Run>(&run::history_awaiting_approval())
        .when(run::approve_plan())
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::Executing);
            assert!(r.plan_approved());
            assert_run_invariants(r);
        });
}

#[test]
fn reject_plan_returns_to_planning_and_counts_revisions() {
    given::<Run>(&run::history_awaiting_approval())
        .when(run::reject_plan())
        .then(&[run::plan_rejected()]);
    let mut history = run::history_awaiting_approval();
    history.push(run::plan_rejected());
    given::<Run>(&history)
        .when(run::propose_plan())
        .then(&[run::plan_proposed_revision(1)]);
    let r = Run::rehydrate(&history);
    assert_eq!(r.status(), RunStatus::Planning);
    assert_eq!(r.plan_revisions(), 1);
    assert!(!r.plan_approved());
}

#[test]
fn start_execution_records_task_ids_once() {
    let mut history = run::history_awaiting_approval();
    history.push(run::plan_approved());
    given::<Run>(&history)
        .when(run::start_execution())
        .then(&[run::execution_started()]);
    given::<Run>(&run::history_executing())
        .when(run::start_execution())
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    given::<Run>(&history)
        .when(StartExecution {
            task_ids: vec![ids::task_id(1)],
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    given::<Run>(&history)
        .when(StartExecution {
            task_ids: vec![ids::task_id(1), ids::task_id(1)],
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let r = Run::rehydrate(&run::history_executing());
    assert_eq!(r.task_ids(), &run::task_ids()[..]);
}

#[test]
fn record_task_usage_rolls_up_and_never_decreases() {
    given::<Run>(&run::history_executing())
        .when(run::record_task_usage())
        .then(&[run::usage_recorded()]);
    let mut history = run::history_executing();
    history.push(run::usage_recorded());
    // Same cumulative usage again is fine; smaller is not.
    given::<Run>(&history)
        .when(run::record_task_usage())
        .then_ok();
    given::<Run>(&history)
        .when(RecordTaskUsage {
            task_id: ids::task_id(1),
            usage: Usage::ZERO,
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let r = Run::rehydrate(&history);
    assert_eq!(r.usage().input_tokens, 3_000);
    assert_eq!(r.task_usage()[&ids::task_id(1)], values::usage());
    assert_run_invariants(&r);
}

#[test]
fn note_task_terminal_moves_to_integrating_when_all_terminal() {
    let planner = values::usage() + values::usage();
    given::<Run>(&run::history_executing())
        .when(run::note_task_succeeded(ids::task_id(1)))
        .then(&[run::task_terminal_noted(
            ids::task_id(1),
            false,
            planner + values::usage(),
        )]);
    let mut history = run::history_executing();
    history.push(run::task_terminal_noted(
        ids::task_id(1),
        false,
        planner + values::usage(),
    ));
    given::<Run>(&history)
        .when(run::note_task_succeeded(ids::task_id(2)))
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::Integrating);
            assert!(r.all_tasks_terminal());
            assert_eq!(r.task_outcomes()[&ids::task_id(2)], TaskOutcome::Succeeded);
            assert_run_invariants(r);
        });
    // Unknown task while executing, duplicate note.
    given::<Run>(&history)
        .when(run::note_task_succeeded(ids::task_id(9)))
        .then_err(DomainError::UnknownTask {
            task_id: ids::task_id(9),
        });
    given::<Run>(&history)
        .when(run::note_task_succeeded(ids::task_id(1)))
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    // Integration tasks (not in the plan) may be noted while integrating.
    given::<Run>(&run::history_integrating())
        .when(NoteTaskTerminal {
            task_id: ids::task_id(7),
            outcome: TaskOutcome::Failed,
            usage: Usage::ZERO,
        })
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::Integrating);
            assert_eq!(r.task_outcomes()[&ids::task_id(7)], TaskOutcome::Failed);
        });
}

#[test]
fn budget_exhausted_is_emitted_once_when_usage_crosses_max_usd() {
    let mut start = run::start_headless();
    start.budget = Budget::unlimited().with_max_usd(usd("0.60"));
    let history = vec![
        run::started_from(&start),
        run::understanding_started(),
        run::understanding_completed(), // 0.25
        run::plan_proposed(),           // 0.50
        run::plan_auto_approved(),
        run::execution_started(),
    ];
    let events = given::<Run>(&history)
        .when(run::record_task_usage()) // +0.25 = 0.75 > 0.60
        .then_ok();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[1],
        RunEvent::BudgetExhausted { dimension: BudgetDimension::Usd, limit, actual }
            if *limit == usd("0.60") && *actual == usd("0.75")
    ));
    let mut exhausted = history.clone();
    exhausted.extend(events);
    let r = Run::rehydrate(&exhausted);
    assert_eq!(
        r.budget_exhausted().map(|e| e.dimension),
        Some(BudgetDimension::Usd)
    );
    assert_eq!(
        r.status(),
        RunStatus::Executing,
        "the saga fails the run after cancelling attempts"
    );
    assert_run_invariants(&r);
    // No second budget_exhausted for further usage.
    let more = given::<Run>(&exhausted)
        .when(RecordTaskUsage {
            task_id: ids::task_id(2),
            usage: values::usage(),
        })
        .then_ok();
    assert_eq!(more.len(), 1);
    // ExhaustBudget (wall, from the saga) is rejected once exhausted.
    given::<Run>(&exhausted)
        .when(run::exhaust_budget())
        .then_invalid_transition();
    // Fail the run with budget_exhausted.
    given::<Run>(&exhausted)
        .when(kevin_domain::run::FailRun {
            reason: RunFailureReason::BudgetExhausted,
            class: FailureClass::Budget,
            message: None,
        })
        .then_state(|r| {
            assert_eq!(r.status(), RunStatus::Failed);
            assert_eq!(r.failure(), Some(&RunFailureReason::BudgetExhausted));
        });
}

#[test]
fn exhaust_budget_from_saga_records_wall_dimension() {
    given::<Run>(&run::history_executing())
        .when(run::exhaust_budget())
        .then(&[run::budget_exhausted()]);
    given::<Run>(&[run::started()])
        .when(run::exhaust_budget())
        .then(&[run::budget_exhausted()]);
}

#[test]
fn mark_integrated_moves_to_evaluating() {
    given::<Run>(&run::history_integrating())
        .when(run::mark_integrated())
        .then(&[run::integrated()]);
    let r = Run::rehydrate(&run::history_evaluating());
    assert_eq!(r.status(), RunStatus::Evaluating);
    assert_eq!(r.artifacts(), &[values::artifact()]);
}

#[test]
fn mark_evaluated_completes_the_run() {
    let usage = values::usage() + values::usage() + values::usage() + values::usage();
    given::<Run>(&run::history_evaluating())
        .when(run::mark_evaluated())
        .then(&[run::evaluated(), run::completed(usage)]);
    let r = Run::rehydrate(&run::history_completed());
    assert_eq!(r.status(), RunStatus::Completed);
    assert_eq!(r.evaluation_ids(), &[ids::evaluation_id()]);
    assert!(r.is_terminal());
    assert_run_invariants(&r);
}

#[test]
fn mark_evaluated_without_evaluation_completes_with_skip_flag() {
    let events = given::<Run>(&run::history_evaluating())
        .when(run::mark_evaluation_skipped())
        .then_ok();
    assert!(matches!(
        &events[..],
        [RunEvent::Completed {
            evaluation_skipped: true,
            ..
        }]
    ));
    let mut bad = run::mark_evaluated();
    if let Some(e) = bad.evaluation.as_mut() {
        e.overall = 1.5;
    }
    given::<Run>(&run::history_evaluating())
        .when(bad)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn terminal_runs_accept_only_evaluate_and_re_evaluation() {
    for history in [
        run::history_completed(),
        {
            let mut h = run::history_executing();
            h.push(run::failed(Usage::ZERO));
            h
        },
        {
            let mut h = run::history_planning();
            h.push(run::cancelled());
            h
        },
    ] {
        given::<Run>(&history)
            .when(run::evaluate())
            .then(&[run::evaluation_requested()]);
        given::<Run>(&history)
            .when(run::mark_evaluated())
            .then(&[run::evaluated()]);
        given::<Run>(&history)
            .when(run::mark_evaluation_skipped())
            .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
        for cmd in non_terminal_commands() {
            given::<Run>(&history).when(cmd).then_invalid_transition();
        }
    }
    // Evaluate is rejected on a live run.
    given::<Run>(&run::history_executing())
        .when(run::evaluate())
        .then_invalid_transition();
}

#[test]
fn cancel_and_fail_from_every_non_terminal_status() {
    for (history, expected_usage) in non_terminal_histories() {
        given::<Run>(&history)
            .when(run::cancel())
            .then(&[run::cancelled()]);
        given::<Run>(&history)
            .when(run::fail())
            .then(&[run::failed(expected_usage)]);
        given::<Run>(&history).when(run::cancel()).then_state(|r| {
            assert_eq!(r.status(), RunStatus::Cancelled);
            assert!(r.is_terminal());
        });
    }
}

#[test]
fn commands_on_a_missing_run_are_not_found() {
    for cmd in non_terminal_commands() {
        given_nothing::<Run>()
            .when(cmd)
            .then_err(DomainError::NotFound {
                aggregate: "run",
                id: uuid::Uuid::nil(),
            });
    }
}

// ---------------------------------------------------------------------------
// Acceptance: every (status × command) cell is covered, accepted or rejected.
// ---------------------------------------------------------------------------

fn non_terminal_histories() -> Vec<(Vec<RunEvent>, Usage)> {
    let u = values::usage();
    let planner = u + u;
    let q = ids::question_id(1);
    let mut approved = run::history_awaiting_approval();
    approved.push(run::plan_approved());
    vec![
        (vec![run::started()], Usage::ZERO),
        (
            vec![run::started(), run::understanding_started()],
            Usage::ZERO,
        ),
        (
            vec![
                run::started(),
                run::understanding_started(),
                run::understanding_completed_with_questions(vec![q]),
            ],
            u,
        ),
        (run::history_planning(), u),
        (run::history_awaiting_approval(), planner),
        (approved, planner),
        (run::history_executing(), planner),
        (run::history_integrating(), planner + u + u),
        (run::history_evaluating(), planner + u + u),
    ]
}

fn non_terminal_commands() -> Vec<RunCommand> {
    vec![
        run::start_understanding().into(),
        run::record_understanding().into(),
        run::note_question_answered(ids::question_id(1)).into(),
        run::propose_plan().into(),
        run::approve_plan().into(),
        run::reject_plan().into(),
        run::start_execution().into(),
        run::record_task_usage().into(),
        run::note_task_succeeded(ids::task_id(1)).into(),
        run::exhaust_budget().into(),
        run::mark_integrated().into(),
        run::cancel().into(),
        run::fail().into(),
    ]
}

#[test]
fn ac_ws01_1_every_run_transition_has_given_when_then_including_rejections() {
    use RunStatus as S;
    // (history label, history)
    let mut histories: Vec<(&str, Vec<RunEvent>)> = vec![("none", vec![])];
    let q = ids::question_id(1);
    histories.push(("received", vec![run::started()]));
    histories.push((
        "understanding",
        vec![run::started(), run::understanding_started()],
    ));
    histories.push((
        "awaiting_answers",
        vec![
            run::started(),
            run::understanding_started(),
            run::understanding_completed_with_questions(vec![q]),
        ],
    ));
    histories.push(("planning", run::history_planning()));
    histories.push(("awaiting_plan_approval", run::history_awaiting_approval()));
    let mut approved = run::history_awaiting_approval();
    approved.push(run::plan_approved());
    histories.push(("executing_fresh", approved));
    histories.push(("executing", run::history_executing()));
    histories.push(("integrating", run::history_integrating()));
    histories.push(("evaluating", run::history_evaluating()));
    histories.push(("completed", run::history_completed()));
    let mut failed = run::history_executing();
    failed.push(run::failed(Usage::ZERO));
    histories.push(("failed", failed));
    let mut cancelled = run::history_planning();
    cancelled.push(run::cancelled());
    histories.push(("cancelled", cancelled));

    // Every command with the history labels in which it is accepted.
    let table: Vec<(RunCommand, &[&str])> = vec![
        (run::start().into(), &["none"]),
        (run::start_understanding().into(), &["received"]),
        (run::record_understanding().into(), &["understanding"]),
        (run::note_question_answered(q).into(), &["awaiting_answers"]),
        (run::propose_plan().into(), &["planning"]),
        (run::approve_plan().into(), &["awaiting_plan_approval"]),
        (run::reject_plan().into(), &["awaiting_plan_approval"]),
        (run::start_execution().into(), &["executing_fresh"]),
        (
            run::record_task_usage().into(),
            &["executing_fresh", "executing", "integrating", "evaluating"],
        ),
        (
            run::note_task_succeeded(ids::task_id(1)).into(),
            &["executing"],
        ),
        (
            run::exhaust_budget().into(),
            &[
                "received",
                "understanding",
                "awaiting_answers",
                "planning",
                "awaiting_plan_approval",
                "executing_fresh",
                "executing",
                "integrating",
                "evaluating",
            ],
        ),
        (run::mark_integrated().into(), &["integrating"]),
        (
            run::mark_evaluated().into(),
            &["evaluating", "completed", "failed", "cancelled"],
        ),
        (
            run::cancel().into(),
            &[
                "received",
                "understanding",
                "awaiting_answers",
                "planning",
                "awaiting_plan_approval",
                "executing_fresh",
                "executing",
                "integrating",
                "evaluating",
            ],
        ),
        (
            run::fail().into(),
            &[
                "received",
                "understanding",
                "awaiting_answers",
                "planning",
                "awaiting_plan_approval",
                "executing_fresh",
                "executing",
                "integrating",
                "evaluating",
            ],
        ),
        (
            run::evaluate().into(),
            &["completed", "failed", "cancelled"],
        ),
    ];

    let mut cells = 0;
    for (cmd, accepted_in) in &table {
        for (label, history) in &histories {
            cells += 1;
            let run = Run::rehydrate(history);
            let result = run.handle(cmd);
            let expect_ok = accepted_in.contains(label);
            assert_eq!(
                result.is_ok(),
                expect_ok,
                "command {} in state {label}: expected {}, got {result:?}",
                cmd.name(),
                if expect_ok { "accepted" } else { "rejected" }
            );
            if let Ok(events) = result {
                let mut after = run.clone();
                for e in &events {
                    after.apply(e);
                }
                assert_run_invariants(&after);
                assert_eq!(after.version(), run.version() + events.len() as u64);
            }
        }
    }
    assert_eq!(cells, table.len() * histories.len());
    // Every status is represented by at least one history.
    for status in S::ALL {
        assert!(
            histories
                .iter()
                .any(|(_, h)| !h.is_empty() && status_of(h) == status),
            "no history reaches {status}"
        );
    }
}

#[test]
fn rehydrate_is_a_pure_function_of_the_stream() {
    let history = run::history_completed();
    let a = Run::rehydrate(&history);
    let b = Run::rehydrate(&history);
    assert_eq!(a.status(), b.status());
    assert_eq!(a.version(), history.len() as u64);
    assert_eq!(a.usage(), b.usage());
    assert_eq!(a.task_ids(), b.task_ids());
    // Executing the same commands from scratch yields the same stream.
    let mut fresh = Run::default();
    let mut stream = Vec::new();
    for cmd in [
        RunCommand::from(run::start()),
        run::start_understanding().into(),
        run::record_understanding().into(),
        run::propose_plan().into(),
        run::approve_plan().into(),
        run::start_execution().into(),
        run::note_task_succeeded(ids::task_id(1)).into(),
        run::note_task_succeeded(ids::task_id(2)).into(),
        run::mark_integrated().into(),
        run::mark_evaluated().into(),
    ] {
        stream.extend(fresh.execute(&cmd).unwrap());
    }
    assert_eq!(stream, history);
    let start: StartRun = run::start();
    assert_eq!(run::started_from(&start), run::started());
    let evaluated: MarkEvaluated = run::mark_evaluated();
    assert!(evaluated.evaluation.is_some());
}
