//! WS-19, CLI half: `kevin proposals ls|show|accept|reject` against a real
//! Postgres (`plan/06-memory-and-learning.md` §3.4, `plan/07-api-and-tui.md`
//! §3). Acceptance criteria 2 and 5 seen from the operator's side.

use std::process::Command;
use std::sync::Arc;

use assert_cmd::prelude::*;
use chrono::Utc;
use kevin_domain::{
    AttemptId, EvaluationId, EvaluationSubject, ModelAlias, Proposal, ProposalId, ProposalKind,
    ProposalStatus, Route, RubricScore, RunId, TaskId, Usage, Verdict, WorkerKind,
};
use kevin_evaluator::{EvaluationRecord, EvaluationRepo, PgEvaluationRepo};
use kevin_store::PgEventStore;
use kevin_testkit::pg::TestDb;
use predicates::prelude::*;

fn kevin(url: &str) -> Command {
    let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
    cmd.env_remove("KEVIN__DATABASE__URL")
        .env_remove("DATABASE_URL")
        .arg("--set")
        .arg(format!("database.url={url}"));
    cmd
}

/// Seeds one evaluation carrying a routing and a prompt proposal.
async fn seed(db: &TestDb) -> (ProposalId, ProposalId) {
    let events = Arc::new(PgEventStore::new(db.pool().clone()));
    let repo = PgEvaluationRepo::new(db.pool().clone(), events);
    let routing = ProposalId::new();
    let prompt = ProposalId::new();
    let record = EvaluationRecord {
        id: EvaluationId::new(),
        subject: EvaluationSubject::Task(TaskId::new()),
        run_id: RunId::new(),
        attempt_id: Some(AttemptId::new()),
        rubric_id: "code".to_owned(),
        judge_route: Route::new(
            WorkerKind::Claude,
            ModelAlias::new("opus5-claude").expect("alias"),
        ),
        scores: vec![RubricScore::new("correctness", 8, "solid").expect("score")],
        overall: 0.8,
        verdict: Verdict::Accept,
        lessons: vec!["run the checks before reporting".to_owned()],
        proposals: vec![
            Proposal {
                id: routing,
                kind: ProposalKind::Routing,
                body: "{\"action\":\"boost\",\"task_kind\":\"implement\",\"alias\":\"sonnet5-claude\",\"quality\":0.9}"
                    .to_owned(),
                rationale: "one-pass implementation".to_owned(),
                status: ProposalStatus::Proposed,
            },
            Proposal {
                id: prompt,
                kind: ProposalKind::Prompt,
                body: "Tell the implementer to run the declared checks.".to_owned(),
                rationale: "two attempts reported done with failing checks".to_owned(),
                status: ProposalStatus::Proposed,
            },
        ],
        usage: Usage::ZERO,
        created_at: Utc::now(),
    };
    repo.record(&record).await.expect("recorded");
    (routing, prompt)
}

#[tokio::test]
async fn ac_ws19_5_proposals_cli_round_trip() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let url = db.url().to_owned();
    let (routing, prompt) = seed(&db).await;

    // ls --json: both proposals wait for a human (acceptance criterion 2).
    let out = kevin(&url)
        .args(["--json", "proposals", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("ls --json is JSON");
    let items = json["items"].as_array().expect("items");
    assert_eq!(items.len(), 2, "{json}");
    assert!(items.iter().all(|p| p["status"] == "proposed"));

    // show
    kevin(&url)
        .args(["proposals", "show", &prompt.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("kind:       prompt"))
        .stdout(predicate::str::contains("run the declared checks"));

    // accept a routing proposal: the event is emitted and the directive applied.
    kevin(&url)
        .args(["proposals", "accept", &routing.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("applied to routing"));
    let outcomes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM routing.route_outcomes WHERE alias = 'sonnet5-claude'",
    )
    .fetch_one(db.pool())
    .await
    .expect("outcome count");
    assert_eq!(outcomes, 1, "the routing directive was applied");

    // accept a prompt proposal: recorded, never applied by Kevin.
    kevin(&url)
        .args(["proposals", "accept", &prompt.to_string()])
        .assert()
        .success()
        .stdout(predicate::str::contains("never written by Kevin"));

    // the inbox is empty, and both decisions are in the event stream.
    let out = kevin(&url)
        .args(["--json", "proposals", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("JSON");
    assert!(json["items"].as_array().expect("items").is_empty());

    let accepted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.events WHERE event_type = 'evaluation.proposal_accepted'",
    )
    .fetch_one(db.pool())
    .await
    .expect("event count");
    assert_eq!(accepted, 2);

    // a decided proposal cannot be decided again.
    kevin(&url)
        .args(["proposals", "reject", &prompt.to_string()])
        .assert()
        .failure();

    db.close().await;
}

#[tokio::test]
async fn rejecting_a_proposal_records_the_decision() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let url = db.url().to_owned();
    let (_, prompt) = seed(&db).await;

    kevin(&url)
        .args([
            "proposals",
            "reject",
            &prompt.to_string(),
            "--note",
            "we already say this",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("rejected"))
        .stdout(predicate::str::contains("we already say this"));

    let status: String = sqlx::query_scalar("SELECT status FROM eval.proposals WHERE id = $1")
        .bind(prompt.as_uuid())
        .fetch_one(db.pool())
        .await
        .expect("status");
    assert_eq!(status, "rejected");
    db.close().await;
}
