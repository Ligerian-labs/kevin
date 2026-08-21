//! Property tests: random command sequences never violate aggregate
//! invariants (one running attempt, attempts ≤ max, budget/usage monotone).

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use kevin_domain::aggregate::Aggregate;
use kevin_domain::kinds::FailureClass;
use kevin_domain::run::{
    CancelRun, ExhaustBudget, FailRun, MarkEvaluated, NoteTaskTerminal, RecordTaskUsage, Run,
    RunCommand, RunStatus, StartExecution, TaskOutcome,
};
use kevin_domain::task::{
    FailAttempt, ProvideInput, RecordProgress, RequestInput, StartAttempt, SucceedAttempt, Task,
    TaskCommand,
};
use kevin_domain::values::{Budget, BudgetDimension, BudgetExcess, RunFailureReason, Usage};
use kevin_testkit::given_when_then::{
    assert_run_invariants, assert_task_invariants, ids, run, task, values,
};
use proptest::prelude::*;

fn arb_usage() -> impl Strategy<Value = Usage> {
    (
        0u64..5_000,
        0u64..5_000,
        0u64..100,
        0u64..100,
        0u64..1_000,
        prop::option::of(0i64..200),
    )
        .prop_map(|(i, o, cr, cw, wall, cents)| Usage {
            input_tokens: i,
            output_tokens: o,
            cache_read_tokens: cr,
            cache_write_tokens: cw,
            cost_usd: cents.map(|c| kevin_domain::Decimal::new(c, 2)),
            wall_ms: wall,
        })
}

fn arb_failure_class() -> impl Strategy<Value = FailureClass> {
    prop::sample::select(FailureClass::ALL.to_vec())
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum RunStep {
    StartInteractive,
    StartHeadless,
    StartUnderstanding,
    RecordUnderstanding {
        questions: u8,
    },
    AnswerQuestion(u8),
    ProposePlan,
    Approve,
    Reject,
    StartExecution,
    RecordUsage {
        task: u8,
        usage: Usage,
    },
    NoteTerminal {
        task: u8,
        succeeded: bool,
        usage: Usage,
    },
    ExhaustWall,
    MarkIntegrated,
    MarkEvaluated,
    MarkEvaluationSkipped,
    Cancel,
    Fail,
    Evaluate,
}

fn arb_run_step() -> impl Strategy<Value = RunStep> {
    prop_oneof![
        1 => Just(RunStep::StartInteractive),
        1 => Just(RunStep::StartHeadless),
        2 => Just(RunStep::StartUnderstanding),
        2 => (0u8..3).prop_map(|questions| RunStep::RecordUnderstanding { questions }),
        2 => (0u8..3).prop_map(RunStep::AnswerQuestion),
        3 => Just(RunStep::ProposePlan),
        2 => Just(RunStep::Approve),
        1 => Just(RunStep::Reject),
        2 => Just(RunStep::StartExecution),
        4 => (0u8..3, arb_usage()).prop_map(|(task, usage)| RunStep::RecordUsage { task, usage }),
        4 => (0u8..3, any::<bool>(), arb_usage())
            .prop_map(|(task, succeeded, usage)| RunStep::NoteTerminal { task, succeeded, usage }),
        1 => Just(RunStep::ExhaustWall),
        2 => Just(RunStep::MarkIntegrated),
        2 => Just(RunStep::MarkEvaluated),
        1 => Just(RunStep::MarkEvaluationSkipped),
        1 => Just(RunStep::Cancel),
        1 => Just(RunStep::Fail),
        1 => Just(RunStep::Evaluate),
    ]
}

fn run_command(step: &RunStep, max_usd_cents: i64) -> RunCommand {
    match step {
        RunStep::StartInteractive => {
            let mut s = run::start();
            s.budget =
                Budget::unlimited().with_max_usd(kevin_domain::Decimal::new(max_usd_cents, 2));
            s.into()
        }
        RunStep::StartHeadless => {
            let mut s = run::start_headless();
            s.budget =
                Budget::unlimited().with_max_usd(kevin_domain::Decimal::new(max_usd_cents, 2));
            s.into()
        }
        RunStep::StartUnderstanding => run::start_understanding().into(),
        RunStep::RecordUnderstanding { questions } => run::record_understanding_with_questions(
            (1..=u128::from(*questions)).map(ids::question_id).collect(),
        )
        .into(),
        RunStep::AnswerQuestion(n) => {
            run::note_question_answered(ids::question_id(u128::from(*n) + 1)).into()
        }
        RunStep::ProposePlan => run::propose_plan().into(),
        RunStep::Approve => run::approve_plan().into(),
        RunStep::Reject => run::reject_plan().into(),
        RunStep::StartExecution => StartExecution {
            task_ids: run::task_ids(),
        }
        .into(),
        RunStep::RecordUsage { task, usage } => RecordTaskUsage {
            task_id: ids::task_id(u128::from(*task) + 1),
            usage: *usage,
        }
        .into(),
        RunStep::NoteTerminal {
            task,
            succeeded,
            usage,
        } => NoteTaskTerminal {
            task_id: ids::task_id(u128::from(*task) + 1),
            outcome: if *succeeded {
                TaskOutcome::Succeeded
            } else {
                TaskOutcome::Failed
            },
            usage: *usage,
        }
        .into(),
        RunStep::ExhaustWall => ExhaustBudget {
            excess: BudgetExcess {
                dimension: BudgetDimension::Wall,
                limit: 1.into(),
                actual: 2.into(),
            },
        }
        .into(),
        RunStep::MarkIntegrated => run::mark_integrated().into(),
        RunStep::MarkEvaluated => run::mark_evaluated().into(),
        RunStep::MarkEvaluationSkipped => MarkEvaluated {
            evaluation: None,
            summary: "skipped".into(),
        }
        .into(),
        RunStep::Cancel => CancelRun {
            by: "p".into(),
            reason: "prop".into(),
        }
        .into(),
        RunStep::Fail => FailRun {
            reason: RunFailureReason::TaskFailed,
            class: FailureClass::Permanent,
            message: None,
        }
        .into(),
        RunStep::Evaluate => run::evaluate().into(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn ac_ws01_2_random_run_command_sequences_never_violate_invariants(
        steps in prop::collection::vec(arb_run_step(), 1..60),
        max_usd_cents in 1i64..2_000,
    ) {
        let mut run = Run::default();
        let mut prev_usage = Usage::ZERO;
        let mut prev_version = 0;
        let mut was_terminal = false;
        let mut prev_status = RunStatus::Received;
        for step in &steps {
            let cmd = run_command(step, max_usd_cents);
            let before = run.clone();
            if let Ok(events) = run.execute(&cmd) {
                prop_assert_eq!(run.version(), prev_version + events.len() as u64);
                // handle is pure: replaying on the clone gives the same events
                prop_assert_eq!(before.handle(&cmd).ok(), Some(events));
            } else {
                prop_assert_eq!(run.version(), prev_version, "rejected commands leave no trace");
                prop_assert_eq!(run.status(), prev_status);
            }
            assert_run_invariants(&run);
            // budget/usage monotone
            let usage = *run.usage();
            prop_assert!(usage.total_tokens() >= prev_usage.total_tokens());
            prop_assert!(usage.wall_ms >= prev_usage.wall_ms);
            if let (Some(now), Some(before_cost)) = (usage.cost_usd, prev_usage.cost_usd) {
                prop_assert!(now >= before_cost);
            }
            prop_assert!(!(prev_usage.cost_usd.is_some() && usage.cost_usd.is_none()));
            // terminal is forever (the only events after are evaluated/requested)
            if was_terminal {
                prop_assert!(run.is_terminal());
                prop_assert_eq!(run.status(), prev_status);
            }
            // executing requires approval
            if run.status() == RunStatus::Executing {
                prop_assert!(run.plan_approved());
            }
            prev_usage = usage;
            prev_version = run.version();
            was_terminal = run.is_terminal();
            prev_status = run.status();
        }
        // rehydrating the stream reproduces the state
        let mut fresh = Run::default();
        let mut replay = Run::default();
        for step in &steps {
            let cmd = run_command(step, max_usd_cents);
            if let Ok(events) = fresh.execute(&cmd) {
                for e in &events {
                    replay.apply(e);
                }
            }
        }
        prop_assert_eq!(replay.status(), run.status());
        prop_assert_eq!(replay.version(), run.version());
        prop_assert_eq!(replay.usage(), run.usage());
    }
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum TaskStep {
    Create { max_attempts: u8 },
    Route,
    Reroute,
    StartAttempt(u8),
    Progress(u8, Usage),
    RequestInput(u8),
    ProvideInput(u8),
    Succeed(u8, Usage),
    Fail(u8, FailureClass, Usage),
    Retry,
    Cancel,
    Skip,
}

fn arb_task_step() -> impl Strategy<Value = TaskStep> {
    prop_oneof![
        1 => (1u8..5).prop_map(|max_attempts| TaskStep::Create { max_attempts }),
        3 => Just(TaskStep::Route),
        1 => Just(TaskStep::Reroute),
        4 => (1u8..6).prop_map(TaskStep::StartAttempt),
        3 => (1u8..6, arb_usage()).prop_map(|(n, u)| TaskStep::Progress(n, u)),
        2 => (1u8..6).prop_map(TaskStep::RequestInput),
        2 => (1u8..6).prop_map(TaskStep::ProvideInput),
        2 => (1u8..6, arb_usage()).prop_map(|(n, u)| TaskStep::Succeed(n, u)),
        3 => (1u8..6, arb_failure_class(), arb_usage()).prop_map(|(n, c, u)| TaskStep::Fail(n, c, u)),
        3 => Just(TaskStep::Retry),
        1 => Just(TaskStep::Cancel),
        1 => Just(TaskStep::Skip),
    ]
}

fn task_command(step: &TaskStep) -> TaskCommand {
    let q = ids::question_id(1);
    match step {
        TaskStep::Create { max_attempts } => {
            let mut c = task::create();
            c.budget = values::budget().with_max_attempts(*max_attempts);
            c.into()
        }
        TaskStep::Route => task::route().into(),
        TaskStep::Reroute => task::reroute().into(),
        TaskStep::StartAttempt(n) => StartAttempt {
            attempt_id: ids::attempt_id(u128::from(*n)),
            workspace: values::workspace(),
            worker_session_id: None,
        }
        .into(),
        TaskStep::Progress(n, usage) => RecordProgress {
            attempt_id: ids::attempt_id(u128::from(*n)),
            summary: "p".into(),
            usage_delta: *usage,
            log_seq: 1,
        }
        .into(),
        TaskStep::RequestInput(n) => RequestInput {
            attempt_id: ids::attempt_id(u128::from(*n)),
            question_id: q,
        }
        .into(),
        TaskStep::ProvideInput(n) => ProvideInput {
            attempt_id: ids::attempt_id(u128::from(*n)),
            question_id: q,
        }
        .into(),
        TaskStep::Succeed(n, usage) => SucceedAttempt {
            attempt_id: ids::attempt_id(u128::from(*n)),
            artifacts: vec![],
            usage: *usage,
            summary: "s".into(),
        }
        .into(),
        TaskStep::Fail(n, class, usage) => FailAttempt {
            attempt_id: ids::attempt_id(u128::from(*n)),
            class: *class,
            message: "f".into(),
            usage: *usage,
        }
        .into(),
        TaskStep::Retry => task::retry().into(),
        TaskStep::Cancel => task::cancel().into(),
        TaskStep::Skip => task::skip().into(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn random_task_command_sequences_keep_one_attempt_and_bounded_retries(
        steps in prop::collection::vec(arb_task_step(), 1..80),
    ) {
        let mut task = Task::default();
        let mut prev_version = 0;
        let mut prev_attempts = 0;
        let mut was_terminal = false;
        for step in &steps {
            let cmd = task_command(step);
            let before = task.clone();
            if let Ok(events) = task.execute(&cmd) {
                prop_assert_eq!(task.version(), prev_version + events.len() as u64);
                prop_assert_eq!(before.handle(&cmd).ok(), Some(events));
            } else {
                prop_assert_eq!(task.version(), prev_version);
            }
            assert_task_invariants(&task);
            let active = task.attempts().iter().filter(|a| a.is_active()).count();
            prop_assert!(active <= 1, "more than one running attempt");
            if task.exists() {
                prop_assert!(task.attempts().len() <= usize::from(task.budget().max_attempts));
            }
            prop_assert!(task.attempts().len() >= prev_attempts, "attempts never disappear");
            if was_terminal {
                prop_assert!(task.is_terminal());
                prop_assert_eq!(task.version(), prev_version, "terminal tasks accept nothing");
            }
            prev_version = task.version();
            prev_attempts = task.attempts().len();
            was_terminal = task.is_terminal();
        }
    }
}
