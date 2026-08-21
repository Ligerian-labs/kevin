//! Given/when/then coverage of the `Task` state machine
//! (`plan/02-domain-model.md` §Task): every transition, every rejection.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use kevin_domain::aggregate::Aggregate;
use kevin_domain::error::DomainError;
use kevin_domain::kinds::FailureClass;
use kevin_domain::task::{
    AttemptStatus, StartAttempt, SucceedAttempt, Task, TaskCommand, TaskEvent, TaskStatus,
};
use kevin_domain::values::Usage;
use kevin_testkit::given_when_then::{
    assert_task_invariants, given, given_nothing, ids, task, values,
};

#[test]
fn create_task_is_pending() {
    given_nothing::<Task>()
        .when(task::create())
        .then(&[task::created()]);
    given_nothing::<Task>()
        .when(task::create())
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Pending);
            assert_eq!(t.task_id(), ids::task_id(1));
            assert_eq!(t.run_id(), ids::run_id());
            assert_eq!(t.kind(), Some(&kevin_domain::kinds::TaskKind::Implement));
            assert!(t.route().is_none());
            assert_task_invariants(t);
        });
    given::<Task>(&[task::created()])
        .when(task::create())
        .then_err(DomainError::AlreadyExists {
            aggregate: "task",
            id: ids::task_id(1).as_uuid(),
        });
    let mut zero = task::create();
    zero.budget.max_attempts = 0;
    given_nothing::<Task>()
        .when(zero)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn route_from_pending_and_reroute_from_routed() {
    given::<Task>(&[task::created()])
        .when(task::route())
        .then(&[task::routed()]);
    given::<Task>(&task::history_routed())
        .when(task::reroute())
        .then(&[task::rerouted()]);
    given::<Task>(&task::history_routed())
        .when(task::reroute())
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Routed);
            assert_eq!(t.route(), Some(&values::other_route()));
        });
    given::<Task>(&task::history_running())
        .when(task::route())
        .then_invalid_transition();
}

#[test]
fn start_attempt_requires_routed_status_and_free_attempts() {
    given::<Task>(&task::history_routed())
        .when(task::start_attempt(1))
        .then(&[task::attempt_started(1)]);
    given::<Task>(&task::history_routed())
        .when(task::start_attempt(1))
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Running);
            assert_eq!(t.attempts().len(), 1);
            assert_eq!(t.active_attempt().map(|a| a.id), Some(ids::attempt_id(1)));
            assert_task_invariants(t);
        });
    // pending: no route yet.
    given::<Task>(&[task::created()])
        .when(task::start_attempt(1))
        .then_invalid_transition();
    // running: one attempt at a time (status guard first, then attempt guard).
    given::<Task>(&task::history_running())
        .when(task::start_attempt(2))
        .then_invalid_transition();
    // attempts exhausted: 2 failures on max_attempts = 2.
    let exhausted = history_failed_final();
    let mut retried = exhausted.clone();
    retried.push(task::retried(3)); // would never be emitted; simulate a routed state
    given::<Task>(&retried)
        .when(task::start_attempt(3))
        .then_err(DomainError::AttemptsExhausted {
            attempts: 2,
            max: 2,
        });
    // attempt id reuse.
    let mut after_retry = task::history_failed_retryable();
    after_retry.push(task::retried(2));
    given::<Task>(&after_retry)
        .when(task::start_attempt(1))
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn progress_accumulates_usage_on_the_active_attempt() {
    given::<Task>(&task::history_running())
        .when(task::record_progress(1))
        .then(&[task::progressed(1)]);
    let mut history = task::history_running();
    history.push(task::progressed(1));
    history.push(task::progressed(1));
    let t = Task::rehydrate(&history);
    assert_eq!(t.usage().input_tokens, 2_000);
    assert_eq!(t.attempts()[0].last_log_seq, 42);
    assert_task_invariants(&t);
    // wrong attempt id
    given::<Task>(&task::history_running())
        .when(task::record_progress(2))
        .then_err(DomainError::AttemptMismatch {
            expected: Some(ids::attempt_id(1)),
            got: ids::attempt_id(2),
        });
    // not running
    given::<Task>(&task::history_routed())
        .when(task::record_progress(1))
        .then_invalid_transition();
}

#[test]
fn input_requested_and_provided_round_trip() {
    let q = ids::question_id(1);
    given::<Task>(&task::history_running())
        .when(task::request_input(1, q))
        .then(&[task::input_requested(1, q)]);
    let mut waiting = task::history_running();
    waiting.push(task::input_requested(1, q));
    let t = Task::rehydrate(&waiting);
    assert_eq!(t.status(), TaskStatus::AwaitingInput);
    assert_eq!(t.active_attempt().and_then(|a| a.pending_question), Some(q));
    assert_task_invariants(&t);
    given::<Task>(&waiting)
        .when(task::provide_input(1, q))
        .then(&[task::input_provided(1, q)]);
    given::<Task>(&waiting)
        .when(task::provide_input(1, q))
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Running);
            assert_task_invariants(t);
        });
    // wrong question / attempt, wrong states
    given::<Task>(&waiting)
        .when(task::provide_input(1, ids::question_id(2)))
        .then_err(DomainError::UnknownQuestion {
            question_id: ids::question_id(2),
        });
    given::<Task>(&waiting)
        .when(task::provide_input(2, q))
        .then_err(DomainError::AttemptMismatch {
            expected: Some(ids::attempt_id(1)),
            got: ids::attempt_id(2),
        });
    given::<Task>(&waiting)
        .when(task::record_progress(1))
        .then_invalid_transition();
    given::<Task>(&waiting)
        .when(task::succeed_attempt(1))
        .then_invalid_transition();
    given::<Task>(&waiting)
        .when(task::request_input(1, q))
        .then_invalid_transition();
    given::<Task>(&task::history_running())
        .when(task::provide_input(1, q))
        .then_invalid_transition();
    // failing while awaiting input is allowed (worker could not resume).
    given::<Task>(&waiting)
        .when(task::fail_attempt(1, FailureClass::Transient))
        .then(&[task::attempt_failed(1, FailureClass::Transient, true)]);
}

#[test]
fn succeed_attempt_terminates_the_task_with_artifacts() {
    given::<Task>(&task::history_running())
        .when(task::succeed_attempt(1))
        .then(&[task::attempt_succeeded(1)]);
    given::<Task>(&task::history_running())
        .when(task::succeed_attempt(1))
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Succeeded);
            assert!(t.is_terminal());
            assert_eq!(t.artifacts(), &[values::artifact()]);
            assert_eq!(t.attempts()[0].status, AttemptStatus::Succeeded);
            assert_eq!(*t.usage(), values::usage());
            assert_task_invariants(t);
        });
    // zero usage on success keeps the accumulated progress usage.
    let mut history = task::history_running();
    history.push(task::progressed(1));
    given::<Task>(&history)
        .when(SucceedAttempt {
            usage: Usage::ZERO,
            ..task::succeed_attempt(1)
        })
        .then_state(|t| assert_eq!(*t.usage(), values::usage()));
    given::<Task>(&task::history_running())
        .when(task::succeed_attempt(2))
        .then_err_matching(|e| matches!(e, DomainError::AttemptMismatch { .. }));
}

#[test]
fn fail_attempt_transient_with_attempts_left_allows_retry() {
    given::<Task>(&task::history_running())
        .when(task::fail_attempt(1, FailureClass::Transient))
        .then(&[task::attempt_failed(1, FailureClass::Transient, true)]);
    let t = Task::rehydrate(&task::history_failed_retryable());
    assert_eq!(t.status(), TaskStatus::Failed);
    assert!(t.can_retry());
    assert!(!t.is_terminal());
    assert_task_invariants(&t);
    // retry → routed (route kept), next attempt no = 2
    given::<Task>(&task::history_failed_retryable())
        .when(task::retry())
        .then(&[task::retried(2)]);
    let mut history = task::history_failed_retryable();
    history.push(task::retried(2));
    let t = Task::rehydrate(&history);
    assert_eq!(t.status(), TaskStatus::Routed);
    assert_eq!(t.route(), Some(&values::route()));
    // re-route to another alias, then start attempt 2
    given::<Task>(&history)
        .when(task::reroute())
        .then(&[task::rerouted()]);
    history.push(task::rerouted());
    let events = given::<Task>(&history)
        .when(task::start_attempt(2))
        .then_ok();
    assert!(matches!(
        &events[..],
        [TaskEvent::AttemptStarted { attempt_no: 2, route, .. }] if *route == values::other_route()
    ));
}

#[test]
fn fail_attempt_runtime_restarted_is_retryable_but_permanent_and_budget_are_not() {
    given::<Task>(&task::history_running())
        .when(task::fail_attempt(1, FailureClass::RuntimeRestarted))
        .then(&[task::attempt_failed(
            1,
            FailureClass::RuntimeRestarted,
            true,
        )]);
    for class in [FailureClass::Permanent, FailureClass::Budget] {
        given::<Task>(&task::history_running())
            .when(task::fail_attempt(1, class))
            .then(&[task::attempt_failed(1, class, false)]);
        let mut history = task::history_running();
        history.push(task::attempt_failed(1, class, false));
        let t = Task::rehydrate(&history);
        assert_eq!(t.status(), TaskStatus::Failed);
        assert!(!t.can_retry());
        assert!(t.is_terminal());
        given::<Task>(&history)
            .when(task::retry())
            .then_err(DomainError::NotRetryable { class });
        given::<Task>(&history)
            .when(task::cancel())
            .then_invalid_transition();
    }
}

#[test]
fn fail_attempt_cancelled_cancels_the_task() {
    let cmd = task::fail_attempt(1, FailureClass::Cancelled);
    given::<Task>(&task::history_running())
        .when(cmd.clone())
        .then(&[
            task::attempt_failed(1, FailureClass::Cancelled, false),
            task::cancelled_with(cmd.message.clone()),
        ]);
    given::<Task>(&task::history_running())
        .when(cmd)
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Cancelled);
            assert!(t.is_terminal());
            assert!(t.active_attempt().is_none());
            assert_task_invariants(t);
        });
}

#[test]
fn attempts_are_bounded_by_max_attempts() {
    let history = history_failed_final();
    let t = Task::rehydrate(&history);
    assert_eq!(t.status(), TaskStatus::Failed);
    assert_eq!(t.attempts_used(), 2);
    assert!(!t.can_retry());
    assert!(t.is_terminal());
    given::<Task>(&history)
        .when(task::retry())
        .then_err(DomainError::AttemptsExhausted {
            attempts: 2,
            max: 2,
        });
    // the second failure was reported with retry_possible = false
    let second = given::<Task>(&history[..history.len() - 1])
        .when(task::fail_attempt(2, FailureClass::Transient))
        .then_ok();
    assert!(matches!(
        &second[..],
        [TaskEvent::AttemptFailed {
            retry_possible: false,
            ..
        }]
    ));
    assert_task_invariants(&t);
}

#[test]
fn cancel_from_every_cancellable_status() {
    let q = ids::question_id(1);
    let mut waiting = task::history_running();
    waiting.push(task::input_requested(1, q));
    for history in [
        vec![task::created()],
        task::history_routed(),
        task::history_running(),
        waiting,
        task::history_failed_retryable(),
    ] {
        given::<Task>(&history)
            .when(task::cancel())
            .then(&[task::cancelled()]);
        given::<Task>(&history)
            .when(task::cancel())
            .then_state(|t| {
                assert_eq!(t.status(), TaskStatus::Cancelled);
                assert!(t.active_attempt().is_none());
                assert_task_invariants(t);
            });
    }
    // cancelling a running task closes the attempt as cancelled
    let mut history = task::history_running();
    history.push(task::cancelled());
    let t = Task::rehydrate(&history);
    assert_eq!(
        t.attempts()[0].failure.as_ref().map(|f| f.class),
        Some(FailureClass::Cancelled)
    );
    // and later worker reports are rejected
    given::<Task>(&history)
        .when(task::fail_attempt(1, FailureClass::Cancelled))
        .then_invalid_transition();
    given::<Task>(&history)
        .when(task::succeed_attempt(1))
        .then_invalid_transition();
}

#[test]
fn skip_only_from_pending() {
    given::<Task>(&[task::created()])
        .when(task::skip())
        .then(&[task::skipped()]);
    given::<Task>(&[task::created()])
        .when(task::skip())
        .then_state(|t| {
            assert_eq!(t.status(), TaskStatus::Skipped);
            assert!(t.is_terminal());
        });
    for history in [
        task::history_routed(),
        task::history_running(),
        task::history_failed_retryable(),
    ] {
        given::<Task>(&history)
            .when(task::skip())
            .then_invalid_transition();
    }
}

#[test]
fn terminal_tasks_reject_everything() {
    let mut succeeded = task::history_running();
    succeeded.push(task::attempt_succeeded(1));
    let mut cancelled = task::history_routed();
    cancelled.push(task::cancelled());
    let mut skipped = vec![task::created()];
    skipped.push(task::skipped());
    for history in [succeeded, cancelled, skipped, history_failed_final()] {
        for cmd in all_non_create_commands() {
            given::<Task>(&history).when(cmd).then_err_matching(|e| {
                matches!(
                    e,
                    DomainError::InvalidTransition { .. }
                        | DomainError::AttemptsExhausted { .. }
                        | DomainError::NotRetryable { .. }
                )
            });
        }
    }
}

#[test]
fn commands_on_a_missing_task_are_not_found() {
    for cmd in all_non_create_commands() {
        given_nothing::<Task>()
            .when(cmd)
            .then_err(DomainError::NotFound {
                aggregate: "task",
                id: uuid::Uuid::nil(),
            });
    }
}

fn history_failed_final() -> Vec<TaskEvent> {
    let mut h = task::history_failed_retryable();
    h.push(task::retried(2));
    h.push(task::attempt_started(2));
    h.push(task::attempt_failed(2, FailureClass::Transient, false));
    h
}

fn all_non_create_commands() -> Vec<TaskCommand> {
    let q = ids::question_id(1);
    vec![
        task::route().into(),
        task::start_attempt(3).into(),
        task::record_progress(1).into(),
        task::request_input(1, q).into(),
        task::provide_input(1, q).into(),
        task::succeed_attempt(1).into(),
        task::fail_attempt(1, FailureClass::Transient).into(),
        task::retry().into(),
        task::cancel().into(),
        task::skip().into(),
    ]
}

// ---------------------------------------------------------------------------
// Acceptance: every (status × command) cell, accepted or rejected.
// ---------------------------------------------------------------------------

#[test]
fn ac_ws01_1_every_task_transition_has_given_when_then_including_rejections() {
    let q = ids::question_id(1);
    let mut waiting = task::history_running();
    waiting.push(task::input_requested(1, q));
    let mut succeeded = task::history_running();
    succeeded.push(task::attempt_succeeded(1));
    let mut cancelled = task::history_routed();
    cancelled.push(task::cancelled());
    let mut skipped = vec![task::created()];
    skipped.push(task::skipped());
    let mut retried = task::history_failed_retryable();
    retried.push(task::retried(2));

    let histories: Vec<(&str, Vec<TaskEvent>)> = vec![
        ("none", vec![]),
        ("pending", vec![task::created()]),
        ("routed", task::history_routed()),
        ("running", task::history_running()),
        ("awaiting_input", waiting),
        ("succeeded", succeeded),
        ("failed_retryable", task::history_failed_retryable()),
        ("routed_after_retry", retried),
        ("failed_final", history_failed_final()),
        ("cancelled", cancelled),
        ("skipped", skipped),
    ];
    let table: Vec<(TaskCommand, &[&str])> = vec![
        (task::create().into(), &["none"]),
        (
            task::route().into(),
            &["pending", "routed", "routed_after_retry"],
        ),
        (
            StartAttempt {
                attempt_id: ids::attempt_id(9),
                ..task::start_attempt(9)
            }
            .into(),
            &["routed", "routed_after_retry"],
        ),
        (task::record_progress(1).into(), &["running"]),
        (task::request_input(1, q).into(), &["running"]),
        (task::provide_input(1, q).into(), &["awaiting_input"]),
        (task::succeed_attempt(1).into(), &["running"]),
        (
            task::fail_attempt(1, FailureClass::Transient).into(),
            &["running", "awaiting_input"],
        ),
        (task::retry().into(), &["failed_retryable"]),
        (
            task::cancel().into(),
            &[
                "pending",
                "routed",
                "running",
                "awaiting_input",
                "failed_retryable",
                "routed_after_retry",
            ],
        ),
        (task::skip().into(), &["pending"]),
    ];

    for (cmd, accepted_in) in &table {
        for (label, history) in &histories {
            let t = Task::rehydrate(history);
            let result = t.handle(cmd);
            let expect_ok = accepted_in.contains(label);
            assert_eq!(
                result.is_ok(),
                expect_ok,
                "command {} in state {label}: expected {}, got {result:?}",
                cmd.name(),
                if expect_ok { "accepted" } else { "rejected" }
            );
            if let Ok(events) = result {
                let mut after = t.clone();
                for e in &events {
                    after.apply(e);
                }
                assert_task_invariants(&after);
            }
        }
    }
    for status in TaskStatus::ALL {
        assert!(
            histories
                .iter()
                .any(|(_, h)| !h.is_empty() && Task::rehydrate(h).status() == status),
            "no history reaches {status}"
        );
    }
}
