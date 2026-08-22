//! Given/when/then coverage of `Question`, `Evaluation`, `RouteScore` and
//! `MemoryItem` (`plan/02-domain-model.md`).

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use kevin_domain::aggregate::Aggregate;
use kevin_domain::error::DomainError;
use kevin_domain::evaluation::{AcceptProposal, Evaluation, RecordEvaluation};
use kevin_domain::kinds::{FailureClass, Tier};
use kevin_domain::memory_item::{MemoryItem, MemoryItemStatus, StoreMemoryItem};
use kevin_domain::question::{AnswerQuestion, AskQuestion, Question};
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome, RouteScore, RouteScoreEvent};
use kevin_domain::values::{
    Answer, ProposalStatus, QuestionOption, QuestionPolicy, QuestionStatus, RubricScore,
};
use kevin_testkit::given_when_then::{
    evaluation, given, given_nothing, ids, memory_item, question, route_score,
};

// ---------------------------------------------------------------------------
// Question
// ---------------------------------------------------------------------------

#[test]
fn ask_question_opens_it() {
    given_nothing::<Question>()
        .when(question::ask())
        .then(&[question::asked()]);
    given_nothing::<Question>()
        .when(question::ask())
        .then_state(|q| {
            assert_eq!(q.status(), QuestionStatus::Open);
            assert!(q.is_open());
            assert_eq!(q.question_id(), ids::question_id(1));
            assert_eq!(q.run_id(), ids::run_id());
            assert_eq!(q.policy(), Some(QuestionPolicy::Block));
            assert_eq!(q.options().len(), 2);
        });
    given::<Question>(&[question::asked()])
        .when(question::ask())
        .then_err(DomainError::AlreadyExists {
            aggregate: "question",
            id: ids::question_id(1).as_uuid(),
        });
    // validation: blank text, duplicate labels, default not an option
    let mut blank = question::ask();
    blank.text = " ".into();
    given_nothing::<Question>()
        .when(blank)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut dup = question::ask();
    dup.options = vec![QuestionOption::new("a"), QuestionOption::new("a")];
    given_nothing::<Question>()
        .when(dup)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut bad_default = question::ask_with_default();
    bad_default.default = Some(Answer::selected(["maybe"], "default"));
    given_nothing::<Question>()
        .when(bad_default)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    assert_eq!(
        question::ask().recommended_default(),
        Some(Answer::selected(["yes"], "default"))
    );
}

#[test]
fn answer_question_once() {
    given::<Question>(&[question::asked()])
        .when(question::answer())
        .then(&[question::answered()]);
    given::<Question>(&[question::asked()])
        .when(question::answer())
        .then_state(|q| {
            assert_eq!(q.status(), QuestionStatus::Answered);
            assert_eq!(
                q.answer().map(|a| a.selected.clone()),
                Some(vec!["no".to_owned()])
            );
        });
    given::<Question>(&[question::asked(), question::answered()])
        .when(question::answer())
        .then_err(DomainError::AlreadyAnswered);
    given::<Question>(&[question::asked(), question::answered()])
        .when(question::expire())
        .then_err(DomainError::AlreadyAnswered);
}

#[test]
fn answer_validation() {
    let asked = [question::asked()];
    // not an option
    given::<Question>(&asked)
        .when(AnswerQuestion {
            answer: Answer::selected(["maybe"], "v"),
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidAnswer { .. }));
    // single-select with two selections
    given::<Question>(&asked)
        .when(AnswerQuestion {
            answer: Answer::selected(["yes", "no"], "v"),
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidAnswer { .. }));
    // empty
    given::<Question>(&asked)
        .when(AnswerQuestion {
            answer: Answer::selected(Vec::<String>::new(), "v"),
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidAnswer { .. }));
    // anonymous
    given::<Question>(&asked)
        .when(AnswerQuestion {
            answer: Answer::selected(["yes"], " "),
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidAnswer { .. }));
    // free text is fine on an options question too (alongside or alone)
    given::<Question>(&asked)
        .when(AnswerQuestion {
            answer: Answer::free_text("only on staging", "v"),
        })
        .then_ok();
    // multi-select accepts two
    let mut multi = question::ask();
    multi.multi_select = true;
    given::<Question>(&[question::asked_from(&multi)])
        .when(AnswerQuestion {
            answer: Answer::selected(["yes", "no"], "v"),
        })
        .then_ok();
}

#[test]
fn expire_with_default_answers_by_default() {
    given::<Question>(&[question::asked_with_default()])
        .when(question::expire())
        .then(&[question::expired(true), question::answered_by_default()]);
    given::<Question>(&[question::asked_with_default()])
        .when(question::expire())
        .then_state(|q| {
            assert_eq!(q.status(), QuestionStatus::Answered);
            assert_eq!(
                q.answer().map(|a| a.answered_by.as_str()),
                Some(Answer::DEFAULT_ANSWERED_BY)
            );
        });
}

#[test]
fn expire_without_default_expires() {
    given::<Question>(&[question::asked_without_default()])
        .when(question::expire())
        .then(&[question::expired(false)]);
    let history = [question::asked_without_default(), question::expired(false)];
    let q = Question::rehydrate(&history);
    assert_eq!(q.status(), QuestionStatus::Expired);
    assert!(q.answer().is_none());
    given::<Question>(&history)
        .when(question::answer())
        .then_invalid_transition();
    given::<Question>(&history)
        .when(question::expire())
        .then_invalid_transition();
}

#[test]
fn blocking_questions_never_expire() {
    given::<Question>(&[question::asked()])
        .when(question::expire())
        .then_err(DomainError::QuestionDoesNotExpire);
}

#[test]
fn question_commands_on_missing_question() {
    for cmd in [
        kevin_domain::question::QuestionCommand::from(question::answer()),
        question::expire().into(),
    ] {
        given_nothing::<Question>()
            .when(cmd)
            .then_err(DomainError::NotFound {
                aggregate: "question",
                id: uuid::Uuid::nil(),
            });
    }
    let ask: AskQuestion = AskQuestion::new(ids::question_id(2), ids::run_id(), "free?");
    given_nothing::<Question>().when(ask).then_ok();
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

#[test]
fn record_evaluation_once_with_valid_scores() {
    given_nothing::<Evaluation>()
        .when(evaluation::record())
        .then(&[evaluation::recorded()]);
    given_nothing::<Evaluation>()
        .when(evaluation::record())
        .then_state(|e| {
            assert_eq!(e.evaluation_id(), ids::evaluation_id());
            assert_eq!(e.scores().len(), 2);
            assert_eq!(e.proposals()[0].status, ProposalStatus::Proposed);
            assert_eq!(e.lessons().len(), 1);
            assert!((e.overall() - 0.85).abs() < f32::EPSILON);
        });
    given::<Evaluation>(&[evaluation::recorded()])
        .when(evaluation::record())
        .then_err(DomainError::AlreadyExists {
            aggregate: "evaluation",
            id: ids::evaluation_id().as_uuid(),
        });
    let mut bad_score = evaluation::record();
    bad_score.scores.push(RubricScore {
        criterion: "x".into(),
        score: 11,
        rationale: String::new(),
    });
    given_nothing::<Evaluation>()
        .when(bad_score)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut bad_overall: RecordEvaluation = evaluation::record();
    bad_overall.overall = 1.2;
    given_nothing::<Evaluation>()
        .when(bad_overall)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut dup = evaluation::record();
    dup.proposals.push(evaluation::proposal_draft());
    given_nothing::<Evaluation>()
        .when(dup)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn proposals_are_decided_once() {
    given::<Evaluation>(&[evaluation::recorded()])
        .when(evaluation::accept_proposal())
        .then(&[evaluation::proposal_accepted()]);
    given::<Evaluation>(&[evaluation::recorded()])
        .when(evaluation::reject_proposal())
        .then(&[evaluation::proposal_rejected()]);
    let accepted = [evaluation::recorded(), evaluation::proposal_accepted()];
    let e = Evaluation::rehydrate(&accepted);
    assert_eq!(
        e.proposal(ids::proposal_id(1)).map(|p| p.status),
        Some(ProposalStatus::Accepted)
    );
    given::<Evaluation>(&accepted)
        .when(evaluation::reject_proposal())
        .then_err(DomainError::ProposalAlreadyDecided {
            proposal_id: ids::proposal_id(1),
            status: ProposalStatus::Accepted,
        });
    given::<Evaluation>(&[evaluation::recorded()])
        .when(AcceptProposal {
            proposal_id: ids::proposal_id(9),
            by: "v".into(),
            note: None,
        })
        .then_err(DomainError::UnknownProposal {
            proposal_id: ids::proposal_id(9),
        });
    given_nothing::<Evaluation>()
        .when(evaluation::accept_proposal())
        .then_err(DomainError::NotFound {
            aggregate: "evaluation",
            id: uuid::Uuid::nil(),
        });
}

// ---------------------------------------------------------------------------
// RouteScore
// ---------------------------------------------------------------------------

#[test]
fn first_outcome_initialises_from_prior_and_updates_beta() {
    given_nothing::<RouteScore>()
        .when(route_score::success())
        .then(&[route_score::score_updated_after_success()]);
    given_nothing::<RouteScore>()
        .when(route_score::success())
        .then_state(|rs| {
            let stats = rs.stats().unwrap();
            assert_eq!(stats.attempts, 1);
            assert_eq!(stats.successes, 1);
            assert!((stats.alpha - 3.0).abs() < f32::EPSILON);
            assert!((stats.beta - 1.0).abs() < f32::EPSILON);
            assert_eq!(stats.mean_cost_usd(), Some("0.40".parse().unwrap()));
            assert_eq!(stats.mean_wall_ms(), Some(60_000));
            assert!((stats.win_rate() - 1.0).abs() < f32::EPSILON);
            assert!((stats.p_success() - 0.75).abs() < f32::EPSILON);
            assert_eq!(
                rs.id(),
                RouteScore::id_for(rs.task_kind().unwrap(), rs.alias().unwrap())
            );
            assert_ne!(rs.id(), uuid::Uuid::nil());
        });
}

#[test]
fn failures_blame_the_model_only_for_permanent_and_budget() {
    let history = [route_score::score_updated_after_success()];
    let after_permanent = given::<RouteScore>(&history)
        .when(route_score::permanent_failure())
        .then_ok();
    let RouteScoreEvent::ScoreUpdated { stats, success, .. } = &after_permanent[0];
    assert_eq!(*success, Some(false));
    assert_eq!(stats.attempts, 2);
    assert_eq!(stats.successes, 1);
    assert!((stats.beta - 2.0).abs() < f32::EPSILON);
    assert_eq!(
        stats.cost_samples, 1,
        "failed attempts do not feed cost means"
    );
    let after_transient = given::<RouteScore>(&history)
        .when(route_score::transient_failure())
        .then_ok();
    let RouteScoreEvent::ScoreUpdated { stats, .. } = &after_transient[0];
    assert_eq!(stats.attempts, 2);
    assert!(
        (stats.beta - 1.0).abs() < f32::EPSILON,
        "transient failures do not move beta"
    );
    // cancelled failure: same as transient
    let cancelled = RecordRouteOutcome {
        failure_class: Some(FailureClass::Cancelled),
        ..route_score::permanent_failure()
    };
    let after_cancel = given::<RouteScore>(&history).when(cancelled).then_ok();
    let RouteScoreEvent::ScoreUpdated { stats, .. } = &after_cancel[0];
    assert!((stats.beta - 1.0).abs() < f32::EPSILON);
}

#[test]
fn quality_ema_and_validation() {
    let mut rs = RouteScore::default();
    rs.execute(&route_score::success().into()).unwrap();
    let second = RecordRouteOutcome {
        quality: Some(0.3),
        ..route_score::success()
    };
    rs.execute(&second.into()).unwrap();
    let stats = rs.stats().unwrap();
    assert!((stats.quality_ema.unwrap() - (0.8 * 0.8 + 0.2 * 0.3)).abs() < 1e-6);
    assert!((stats.mean_quality().unwrap() - 0.55).abs() < 1e-6);
    // bad quality / cost / prior
    given_nothing::<RouteScore>()
        .when(RecordRouteOutcome {
            quality: Some(1.5),
            ..route_score::success()
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    given_nothing::<RouteScore>()
        .when(RecordRouteOutcome {
            cost_usd: Some("-1".parse().unwrap()),
            ..route_score::success()
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    given_nothing::<RouteScore>()
        .when(RecordRouteOutcome {
            prior: BetaPrior {
                alpha: 0.5,
                beta: 1.0,
            },
            ..route_score::success()
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    // wrong pair on an existing stream
    given::<RouteScore>(&[route_score::score_updated_after_success()])
        .when(RecordRouteOutcome {
            task_kind: kevin_domain::kinds::TaskKind::Test,
            ..route_score::success()
        })
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
}

#[test]
fn reset_returns_to_prior_keeping_last_used() {
    let history = [route_score::score_updated_after_success()];
    let events = given::<RouteScore>(&history)
        .when(route_score::reset())
        .then_ok();
    let RouteScoreEvent::ScoreUpdated {
        stats,
        reset,
        success,
        ..
    } = &events[0];
    assert!(*reset);
    assert_eq!(*success, None);
    assert_eq!(stats.attempts, 0);
    assert!((stats.alpha - BetaPrior::for_tier(Tier::Balanced).alpha).abs() < f32::EPSILON);
    assert!(stats.last_used.is_some());
    // reset on a fresh stream is fine too
    given_nothing::<RouteScore>()
        .when(route_score::reset())
        .then_ok();
}

// ---------------------------------------------------------------------------
// MemoryItem
// ---------------------------------------------------------------------------

#[test]
fn store_supersede_forget() {
    given_nothing::<MemoryItem>()
        .when(memory_item::store())
        .then(&[memory_item::stored()]);
    given_nothing::<MemoryItem>()
        .when(memory_item::store())
        .then_state(|m| {
            assert_eq!(m.status(), MemoryItemStatus::Active);
            assert!(m.is_active());
            assert_eq!(m.memory_item_id(), ids::memory_item_id(1));
            assert_eq!(m.kind(), Some(kevin_domain::values::MemoryKind::Lesson));
            assert_eq!(m.scope().to_string(), "repo:abc");
            assert_eq!(m.embedding_model(), Some("BAAI/bge-small-en-v1.5"));
        });
    given::<MemoryItem>(&[memory_item::stored()])
        .when(memory_item::supersede())
        .then(&[memory_item::superseded()]);
    let superseded = [memory_item::stored(), memory_item::superseded()];
    let m = MemoryItem::rehydrate(&superseded);
    assert_eq!(m.status(), MemoryItemStatus::Superseded);
    assert_eq!(m.superseded_by(), Some(ids::memory_item_id(2)));
    given::<MemoryItem>(&superseded)
        .when(memory_item::supersede())
        .then_invalid_transition();
    given::<MemoryItem>(&superseded)
        .when(memory_item::forget())
        .then(&[memory_item::forgotten()]);
    given::<MemoryItem>(&[memory_item::stored()])
        .when(memory_item::forget())
        .then(&[memory_item::forgotten()]);
    let forgotten = [memory_item::stored(), memory_item::forgotten()];
    let m = MemoryItem::rehydrate(&forgotten);
    assert_eq!(m.status(), MemoryItemStatus::Forgotten);
    assert_eq!(m.content(), "", "forgotten items keep no content");
    given::<MemoryItem>(&forgotten)
        .when(memory_item::forget())
        .then_invalid_transition();
    given::<MemoryItem>(&forgotten)
        .when(memory_item::supersede())
        .then_invalid_transition();
}

#[test]
fn store_validation_and_missing_item() {
    given::<MemoryItem>(&[memory_item::stored()])
        .when(memory_item::store())
        .then_err(DomainError::AlreadyExists {
            aggregate: "memory_item",
            id: ids::memory_item_id(1).as_uuid(),
        });
    let mut empty: StoreMemoryItem = memory_item::store();
    empty.content = "  ".into();
    given_nothing::<MemoryItem>()
        .when(empty)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut long = memory_item::store();
    long.content = "x".repeat(8001);
    given_nothing::<MemoryItem>()
        .when(long)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut important = memory_item::store();
    important.importance = 1.5;
    given_nothing::<MemoryItem>()
        .when(important)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    let mut self_ref = memory_item::supersede();
    self_ref.superseded_by = ids::memory_item_id(1);
    given::<MemoryItem>(&[memory_item::stored()])
        .when(self_ref)
        .then_err_matching(|e| matches!(e, DomainError::InvalidValue(_)));
    given_nothing::<MemoryItem>()
        .when(memory_item::forget())
        .then_err(DomainError::NotFound {
            aggregate: "memory_item",
            id: uuid::Uuid::nil(),
        });
}
