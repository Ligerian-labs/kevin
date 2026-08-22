//! WS-19 acceptance tests (`plan/12-workstreams.md` WS-19), the ones that need
//! no database. The judge always runs on the fake worker.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    AutoApplyParts, EXECUTOR_ALIAS, Fixture, GOLDEN_ACCEPT, GOLDEN_GENEROUS, GOLDEN_RUN,
    OTHER_JUDGE_ALIAS, alias, config, executor_route, implement, implement_evidence, task_ids,
};
use kevin_config::Evaluation as EvaluationCfg;
use kevin_domain::{
    EvaluationSubject, ProposalKind, ProposalStatus, TaskKind, Verdict, WorkerKind,
};
use kevin_evaluator::{
    EvaluationRepo, EvaluationRequest, Evaluator, OutcomeAttempt, Proposals, Rubric, RubricError,
    SkipReason,
};

/// AC 1 — a golden judge output produces `evaluation.recorded`, one route
/// outcome and the lessons, with `overall` recomputed from the rubric weights.
#[tokio::test]
async fn ac_ws19_1_golden_judge_output_records_evaluation_route_outcomes_and_lessons() {
    let fx = Fixture::new(GOLDEN_ACCEPT);
    let (run_id, task_id) = task_ids();
    let attempt = OutcomeAttempt::new(run_id, task_id, kevin_domain::AttemptId::new());
    let request = EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
        .with_attempt(attempt)
        .with_executor_route(executor_route());

    let outcome = fx
        .evaluator
        .evaluate_detailed(request)
        .await
        .expect("judged");

    // The evaluation itself.
    let record = &outcome.record;
    assert_eq!(record.rubric_id, "code", "implement → the `code` rubric");
    assert_eq!(record.subject, EvaluationSubject::Task(task_id));
    // 0.25*0.9 + 0.20*0.8 + 0.15*0.8 + 0.15*0.7 + 0.15*1.0 + 0.10*0.6
    assert!(
        (record.overall - 0.82).abs() < 1e-5,
        "overall is recomputed from the weights, not taken from the judge: {}",
        record.overall
    );
    assert!(
        (outcome.judge.overall - 0.95).abs() < 1e-6,
        "the judge's own arithmetic is kept for the log"
    );
    assert_eq!(record.verdict, Verdict::Accept);
    assert_eq!(record.scores.len(), 6);
    assert_eq!(record.lessons.len(), 2);

    // `evaluation.recorded` was appended.
    let events = fx.repo.events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        kevin_domain::EvaluationEvent::Recorded { overall, verdict, .. }
            if (*overall - record.overall).abs() < f32::EPSILON && *verdict == Verdict::Accept
    ));
    let stored = fx
        .repo
        .evaluation(record.id)
        .await
        .unwrap()
        .expect("projected");
    assert!((stored.overall - record.overall).abs() < f32::EPSILON);

    // Route outcome: quality = the recomputed overall, keyed by the attempt.
    let outcomes = fx.router.outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].task_kind, TaskKind::Implement);
    assert_eq!(outcomes[0].alias, alias(EXECUTOR_ALIAS));
    assert_eq!(outcomes[0].quality, Some(record.overall));
    assert!(outcomes[0].success);
    assert_eq!(
        fx.router.attempts()[0].map(|a| a.attempt_id),
        Some(attempt.attempt_id)
    );

    // Lessons landed in memory.
    assert_eq!(fx.memory.lessons().len(), 2);
    assert!(
        fx.memory
            .lessons()
            .iter()
            .any(|l| l.contains("repository checks"))
    );
    assert_eq!(outcome.applied.route_outcomes, 1);
    assert_eq!(outcome.applied.lessons_stored, 2);
}

/// AC 1 (continued) — a generous judge cannot buy an `accept`: the weighted
/// score is recomputed server-side and the stricter verdict wins.
#[tokio::test]
async fn ac_ws19_1b_a_generous_judge_cannot_overrule_the_weighted_score() {
    let fx = Fixture::new(GOLDEN_GENEROUS);
    let (run_id, task_id) = task_ids();
    let request = EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
        .with_executor_route(executor_route());

    let outcome = fx
        .evaluator
        .evaluate_detailed(request)
        .await
        .expect("judged");
    assert_eq!(outcome.judge.verdict, Verdict::Accept);
    assert!((outcome.record.overall - 0.4).abs() < 1e-5);
    assert_eq!(
        outcome.record.verdict,
        Verdict::Reject,
        "0.4 → reject; the stricter of the two verdicts wins"
    );
}

/// AC 2 — proposals are never auto-applied: they land in the inbox as
/// `proposed`, whatever `evaluation.auto_apply` says.
#[tokio::test]
async fn ac_ws19_2_proposals_are_never_auto_applied() {
    let fx = Fixture::new(GOLDEN_ACCEPT);
    let (run_id, task_id) = task_ids();
    let request = EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
        .with_executor_route(executor_route());

    let outcome = fx
        .evaluator
        .evaluate_detailed(request)
        .await
        .expect("judged");
    assert_eq!(outcome.applied.proposals_raised, 2);

    let inbox = fx
        .repo
        .proposals(Some(ProposalStatus::Proposed), 50)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 2);
    assert!(inbox.iter().all(|p| p.status == ProposalStatus::Proposed));
    assert!(inbox.iter().any(|p| p.kind == ProposalKind::Routing));
    assert!(inbox.iter().any(|p| p.kind == ProposalKind::Prompt));

    // The routing *proposal* was not applied: the only route outcome recorded is
    // the evaluation's own, whose quality is the recomputed overall (0.82), not
    // the proposal's 0.85.
    let outcomes = fx.router.outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].quality, Some(outcome.record.overall));

    // With auto-apply narrowed to nothing, proposals are still raised and
    // nothing else is touched.
    let strict = Fixture::with(GOLDEN_ACCEPT, config(true), AutoApplyParts::None);
    let (run_id, task_id) = task_ids();
    let outcome = strict
        .evaluator
        .evaluate_detailed(
            EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                .with_executor_route(executor_route()),
        )
        .await
        .expect("judged");
    assert_eq!(outcome.applied.proposals_raised, 2);
    assert_eq!(outcome.applied.route_outcomes, 0);
    assert_eq!(outcome.applied.lessons_stored, 0);
    assert!(strict.router.is_empty());
    assert!(strict.memory.lessons().is_empty());
    assert_eq!(
        strict
            .repo
            .proposals(Some(ProposalStatus::Proposed), 50)
            .await
            .unwrap()
            .len(),
        2
    );
}

/// AC 3 — the judge runs on a different `worker + model` than the executor when
/// two judge-capable aliases exist, and its evidence never names a route.
#[tokio::test]
async fn ac_ws19_3_judge_route_differs_from_the_executor_route_when_candidates_allow() {
    let fx = Fixture::new(GOLDEN_ACCEPT);
    let executor = executor_route();
    let judge = fx.evaluator.judge_route(Some(&executor));
    assert_eq!(fx.evaluator.judge_candidates().len(), 2);
    assert_ne!(judge.model, executor.model);
    assert_eq!(judge.model, alias(OTHER_JUDGE_ALIAS));

    // With a single candidate there is nothing to switch to: `[roles].judge`.
    let single = Fixture::with(GOLDEN_ACCEPT, config(false), AutoApplyParts::Both);
    let judge = single.evaluator.judge_route(Some(&executor));
    assert_eq!(judge.model, executor.model);
    assert_eq!(judge.worker, WorkerKind::Fake);

    // The recorded evaluation carries the judge's own route, not the executor's.
    let (run_id, task_id) = task_ids();
    let outcome = fx
        .evaluator
        .evaluate_detailed(
            EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                .with_executor_route(executor.clone()),
        )
        .await
        .expect("judged");
    assert_eq!(outcome.record.judge_route.model, alias(OTHER_JUDGE_ALIAS));

    // Anti-gaming: the prompt the judge received names no alias, model or worker.
    let ctx = kevin_evaluator::JudgeContext::new(
        Rubric::builtin("code").unwrap(),
        implement_evidence()
            .with_diff("diff --git a/x b/x\n+ generated by fake-executor on the fake worker"),
    );
    let scrubbed =
        kevin_evaluator::Judge.build(&ctx.with_scrubber(kevin_evaluator::Scrubber::new([
            "fake-executor",
            "fake",
            "fake-judge-model",
        ])));
    let lower = scrubbed.user.to_lowercase();
    assert!(!lower.contains("fake-executor"));
    assert!(!lower.contains("fake-judge-model"));
}

/// AC 4 — every rubric's weights sum to 1, and one that does not is rejected at
/// load (so it never reaches a judge).
#[tokio::test]
async fn ac_ws19_4_rubric_weights_sum_to_one() {
    for (id, _) in kevin_evaluator::rubric::BUILTINS {
        let rubric = Rubric::builtin(id).expect("built-in rubric");
        let sum = rubric.weight_sum();
        assert!(
            (sum - 1.0).abs() <= kevin_evaluator::rubric::WEIGHT_EPSILON,
            "rubric `{id}` weights sum to {sum}"
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(
        &bad,
        "id = \"bad\"\n[[criteria]]\nkey = \"a\"\nweight = 0.6\n[[criteria]]\nkey = \"b\"\nweight = 0.6\n",
    )
    .unwrap();
    let err = Rubric::load(&bad).expect_err("rejected");
    assert!(matches!(err, RubricError::WeightSum { .. }), "{err}");

    // An evaluator configured with it fails before calling any worker.
    let mut cfg = config(true);
    cfg.evaluation = EvaluationCfg {
        rubric: bad.to_string_lossy().into_owned(),
        ..EvaluationCfg::default()
    };
    let fx = Fixture::with(GOLDEN_ACCEPT, cfg, AutoApplyParts::Both);
    let run_id = kevin_domain::RunId::new();
    let err = fx
        .evaluator
        .evaluate(EvaluationRequest::for_run(run_id, implement_evidence()))
        .await
        .expect_err("bad rubric");
    assert!(
        matches!(err, kevin_evaluator::EvaluatorError::Rubric(_)),
        "{err}"
    );
    assert!(fx.repo.events().is_empty());
}

/// AC 5 — `kevin proposals accept` emits `evaluation.proposal_accepted` and,
/// for a routing proposal, applies it; prompt/config proposals only ever print
/// what a human must do.
#[tokio::test]
async fn ac_ws19_5_accepting_a_proposal_emits_the_event_and_applies_routing() {
    let fx = Fixture::new(GOLDEN_ACCEPT);
    let (run_id, task_id) = task_ids();
    fx.evaluator
        .evaluate(
            EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                .with_executor_route(executor_route()),
        )
        .await
        .expect("judged");

    let router = Arc::new(kevin_evaluator::InMemoryRouter::new());
    let inbox = Proposals::new(fx.repo.clone()).with_router(router.clone());
    let rows = inbox
        .list(Some(ProposalStatus::Proposed), 50)
        .await
        .unwrap();
    let routing = rows
        .iter()
        .find(|p| p.kind == ProposalKind::Routing)
        .expect("routing proposal");
    let prompt = rows
        .iter()
        .find(|p| p.kind == ProposalKind::Prompt)
        .expect("prompt proposal");

    // Routing: event + applied.
    let accepted = inbox
        .accept(routing.id, "vale", Some("looks right".to_owned()))
        .await
        .expect("accepted");
    assert_eq!(accepted.proposal.status, ProposalStatus::Accepted);
    assert!(accepted.applied, "a routing directive is applied on accept");
    assert!(accepted.manual.is_none());
    assert_eq!(router.len(), 1);
    assert_eq!(router.outcomes()[0].quality, Some(0.85));
    assert_eq!(router.outcomes()[0].task_kind, TaskKind::Implement);
    assert!(
        fx.repo.events().iter().any(|e| matches!(
            e,
            kevin_domain::EvaluationEvent::ProposalAccepted { proposal_id, by, note }
                if *proposal_id == routing.id
                    && by == "vale"
                    && note.as_deref() == Some("looks right")
        )),
        "evaluation.proposal_accepted was emitted, with the operator note"
    );

    // Prompt: event, never applied.
    let accepted = inbox
        .accept(prompt.id, "vale", None)
        .await
        .expect("accepted");
    assert!(!accepted.applied);
    assert!(accepted.manual.is_some());
    assert_eq!(router.len(), 1, "no route outcome from a prompt proposal");

    // A decided proposal cannot be decided again.
    assert!(inbox.reject(prompt.id, "vale", None).await.is_err());
    assert!(
        inbox
            .list(Some(ProposalStatus::Proposed), 50)
            .await
            .unwrap()
            .is_empty()
    );
}

/// `evaluation.evaluate_tasks = false` gates task evaluations; run evaluations
/// still happen (`plan/06-memory-and-learning.md` §3.3).
#[tokio::test]
async fn task_evaluations_follow_the_evaluate_tasks_gate() {
    let mut cfg = config(true);
    cfg.evaluation = EvaluationCfg {
        evaluate_tasks: false,
        ..EvaluationCfg::default()
    };
    let fx = Fixture::with(GOLDEN_RUN, cfg, AutoApplyParts::Both);
    let (run_id, task_id) = task_ids();
    assert!(!fx.evaluator.will_evaluate(EvaluationSubject::Task(task_id)));
    assert!(fx.evaluator.will_evaluate(EvaluationSubject::Run(run_id)));

    let err = fx
        .evaluator
        .evaluate(EvaluationRequest::for_task(
            run_id,
            task_id,
            implement(),
            implement_evidence(),
        ))
        .await
        .expect_err("gated");
    assert!(err.is_skipped());
    assert!(matches!(
        err,
        kevin_evaluator::EvaluatorError::Skipped(SkipReason::TasksDisabled)
    ));
    assert!(fx.repo.events().is_empty());
    assert!(fx.router.is_empty());

    // A run evaluation still runs, on the configured rubric.
    let id = fx
        .evaluator
        .evaluate(EvaluationRequest::for_run(run_id, implement_evidence()))
        .await
        .expect("run judged");
    let record = fx.repo.evaluation(id).await.unwrap().expect("recorded");
    assert_eq!(record.rubric_id, "default");
    // 0.30*0.8 + 0.25*0.7 + 0.15*0.8 + 0.15*0.9 + 0.15*0.6 = 0.76
    assert!((record.overall - 0.76).abs() < 1e-5, "{}", record.overall);
    assert_eq!(record.verdict, Verdict::AcceptWithFixes);
}

/// `evaluation.enabled = false` switches the whole context off.
#[tokio::test]
async fn a_disabled_evaluator_judges_nothing() {
    let mut cfg = config(true);
    cfg.evaluation = EvaluationCfg {
        enabled: false,
        ..EvaluationCfg::default()
    };
    let fx = Fixture::with(GOLDEN_ACCEPT, cfg, AutoApplyParts::Both);
    let run_id = kevin_domain::RunId::new();
    let err = fx
        .evaluator
        .evaluate(EvaluationRequest::for_run(run_id, implement_evidence()))
        .await
        .expect_err("disabled");
    assert!(matches!(
        err,
        kevin_evaluator::EvaluatorError::Skipped(SkipReason::Disabled)
    ));
}

/// The judge repairs a schema violation exactly once, then gives up.
#[tokio::test]
async fn a_judge_that_breaks_its_schema_twice_fails_the_call() {
    let fx = Fixture::new("{\"scores\": [], \"overall\": 2}");
    let run_id = kevin_domain::RunId::new();
    let err = tokio::time::timeout(
        Duration::from_secs(20),
        fx.evaluator
            .evaluate(EvaluationRequest::for_run(run_id, implement_evidence())),
    )
    .await
    .expect("no hang")
    .expect_err("unusable answer");
    assert!(
        matches!(
            err,
            kevin_evaluator::EvaluatorError::JudgeOutput(_)
                | kevin_evaluator::EvaluatorError::JudgeFailed { .. }
        ),
        "{err}"
    );
    assert!(fx.repo.events().is_empty());
}

/// The evaluator type is the frozen entry point of the crate.
#[test]
fn the_frozen_surface_is_present() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Evaluator>();
    assert_send_sync::<Proposals>();
}
