//! WS-19 acceptance tests that need Postgres: the `eval` schema projections and
//! the aggregate's event stream (`plan/11-testing.md` — `TestDb` +
//! `skip_unless_pg!`).

mod common;

use std::sync::Arc;

use common::{
    GOLDEN_ACCEPT, config, executor_route, implement, implement_evidence, registry, task_ids,
};
use kevin_domain::{ProposalKind, ProposalStatus, TaskKind, Verdict};
use kevin_evaluator::{
    AutoApply, EvaluationRepo, EvaluationRequest, Evaluator, InMemoryLessons, InMemoryRouter,
    OutcomeAttempt, PgEvaluationRepo, Proposals,
};
use kevin_store::PgEventStore;
use kevin_testkit::pg::TestDb;
use kevin_testkit::skip_unless_pg;
use sqlx::Row as _;

/// AC 1 (Postgres) — a golden judge output lands in `eval.evaluations` and
/// `eval.proposals`, and `evaluation.recorded` is in `core.events`.
#[tokio::test]
async fn ac_ws19_1_pg_evaluation_and_proposals_are_projected() {
    skip_unless_pg!();
    let db = TestDb::new().await;
    let events = Arc::new(PgEventStore::new(db.pool().clone()));
    let repo = Arc::new(PgEvaluationRepo::new(db.pool().clone(), events));
    let router = Arc::new(InMemoryRouter::new());
    let memory = Arc::new(InMemoryLessons::new());

    let (dir, workers) = registry(GOLDEN_ACCEPT, true);
    let evaluator = Evaluator::new(
        config(true),
        Arc::new(workers),
        kevin_worker::Workspace::in_place(dir.path()),
        repo.clone(),
        AutoApply::new([
            kevin_config::AutoApply::Routing,
            kevin_config::AutoApply::Memory,
        ])
        .with_router(router.clone())
        .with_memory(memory.clone()),
    );

    let (run_id, task_id) = task_ids();
    let attempt = OutcomeAttempt::new(run_id, task_id, kevin_domain::AttemptId::new());
    let id = evaluator
        .evaluate(
            EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                .with_attempt(attempt)
                .with_executor_route(executor_route()),
        )
        .await
        .expect("judged");

    // The projection round-trips.
    let record = repo.evaluation(id).await.unwrap().expect("row");
    assert_eq!(record.rubric_id, "code");
    assert_eq!(record.verdict, Verdict::Accept);
    assert!((record.overall - 0.82).abs() < 1e-5);
    assert_eq!(record.scores.len(), 6);
    assert_eq!(record.lessons.len(), 2);
    assert_eq!(record.attempt_id, Some(attempt.attempt_id));
    assert_eq!(record.proposals.len(), 2);

    // The row is what the DDL says it is.
    let row = sqlx::query(
        "SELECT subject_type, run_id, judge_alias, judge_worker, verdict, array_length(lessons, 1) AS lessons \
         FROM eval.evaluations WHERE id = $1",
    )
    .bind(id.as_uuid())
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("subject_type"), "task");
    assert_eq!(row.get::<uuid::Uuid, _>("run_id"), run_id.as_uuid());
    assert_eq!(row.get::<String, _>("judge_worker"), "fake");
    assert_eq!(row.get::<String, _>("verdict"), "accept");
    assert_eq!(row.get::<i32, _>("lessons"), 2);

    // Both proposals are in the inbox as `proposed` (AC 2 through Postgres).
    let inbox = repo
        .proposals(Some(ProposalStatus::Proposed), 50)
        .await
        .unwrap();
    assert_eq!(inbox.len(), 2);
    assert!(inbox.iter().all(|p| p.run_id == run_id));

    // `evaluation.recorded` is the aggregate's only event so far.
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.events WHERE aggregate_type = 'evaluation' AND aggregate_id = $1",
    )
    .bind(id.as_uuid())
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(count, 1);

    // Auto-apply still ran.
    assert_eq!(router.len(), 1);
    assert_eq!(memory.lessons().len(), 2);
    db.close().await;
}

/// AC 5 (Postgres) — accepting a routing proposal appends
/// `evaluation.proposal_accepted`, flips the row and applies the directive.
#[tokio::test]
async fn ac_ws19_5_pg_accepting_a_proposal_emits_the_event_and_applies() {
    skip_unless_pg!();
    let db = TestDb::new().await;
    let events = Arc::new(PgEventStore::new(db.pool().clone()));
    let repo = Arc::new(PgEvaluationRepo::new(db.pool().clone(), events));

    let (dir, workers) = registry(GOLDEN_ACCEPT, true);
    let evaluator = Evaluator::new(
        config(true),
        Arc::new(workers),
        kevin_worker::Workspace::in_place(dir.path()),
        repo.clone(),
        AutoApply::none(),
    );
    let (run_id, task_id) = task_ids();
    let id = evaluator
        .evaluate(
            EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                .with_executor_route(executor_route()),
        )
        .await
        .expect("judged");

    let router = Arc::new(InMemoryRouter::new());
    let inbox = Proposals::new(repo.clone()).with_router(router.clone());
    let rows = inbox
        .list(Some(ProposalStatus::Proposed), 50)
        .await
        .unwrap();
    let routing = rows
        .iter()
        .find(|p| p.kind == ProposalKind::Routing)
        .expect("routing proposal");

    let accepted = inbox
        .accept(routing.id, "vale", None)
        .await
        .expect("accepted");
    assert!(accepted.applied);
    assert_eq!(router.len(), 1);
    assert_eq!(router.outcomes()[0].task_kind, TaskKind::Implement);

    let row = sqlx::query("SELECT status, decided_by FROM eval.proposals WHERE id = $1")
        .bind(routing.id.as_uuid())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("status"), "accepted");
    assert_eq!(
        row.get::<Option<String>, _>("decided_by").as_deref(),
        Some("vale")
    );

    let types: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM core.events WHERE aggregate_id = $1 ORDER BY aggregate_version",
    )
    .bind(id.as_uuid())
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        types,
        vec!["evaluation.recorded", "evaluation.proposal_accepted"]
    );

    // The inbox filter is the `eval.proposals_inbox` read model of plan/02.
    let remaining = inbox
        .list(Some(ProposalStatus::Proposed), 50)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].kind, ProposalKind::Prompt);
    db.close().await;
}
