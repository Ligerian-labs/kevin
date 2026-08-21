//! WS-09 acceptance criterion (6): `kevin routes` prints the leaderboard and
//! `kevin routes explain` prints the candidate table of a dry-run selection
//! (`plan/06-memory-and-learning.md` §2.5).

use std::process::Command;
use std::sync::Arc;

use assert_cmd::prelude::*;
use chrono::Utc;
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome};
use kevin_domain::{FailureClass, ModelAlias, TaskKind};
use kevin_router::{AttemptRef, PgRouteScoreRepo, RouteScoreRepo};
use kevin_testkit::pg::TestDb;
use predicates::prelude::*;
use uuid::Uuid;

fn kevin(url: &str) -> Command {
    let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
    cmd.env_remove("DATABASE_URL")
        .env("KEVIN__DATABASE__URL", url);
    cmd
}

fn alias(name: &str) -> ModelAlias {
    ModelAlias::new(name).expect("valid alias")
}

fn outcome(alias_name: &str, success: bool) -> RecordRouteOutcome {
    RecordRouteOutcome {
        task_kind: TaskKind::Implement,
        alias: alias(alias_name),
        success,
        quality: Some(if success { 0.85 } else { 0.2 }),
        cost_usd: None,
        wall_ms: 372_000,
        failure_class: (!success).then_some(FailureClass::Permanent),
        recorded_at: Utc::now(),
        prior: BetaPrior::UNIFORM,
    }
}

/// A test database with four successful `sonnet5-claude` outcomes and one
/// failed `gpt56-codex` outcome on `implement`.
async fn seeded_db() -> (TestDb, Arc<PgRouteScoreRepo>) {
    let db = TestDb::new().await;
    let repo = Arc::new(PgRouteScoreRepo::new(db.pool().clone()));
    for _ in 0..4 {
        repo.record(
            &outcome("sonnet5-claude", true),
            Some(AttemptRef::new(
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
            )),
            "cli-catalog",
        )
        .await
        .expect("record");
    }
    repo.record(&outcome("gpt56-codex", false), None, "cli-catalog")
        .await
        .expect("record");
    (db, repo)
}

#[tokio::test]
async fn ac_ws09_6_routes_prints_the_leaderboard() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let url = db.url().to_owned();

    // Empty leaderboard first: the command works before anything was learned.
    kevin(&url)
        .arg("routes")
        .assert()
        .success()
        .stdout(predicate::str::contains("no route scores yet"));
    db.close().await;

    let (db, _repo) = seeded_db().await;
    let url = db.url().to_owned();
    kevin(&url)
        .arg("routes")
        .assert()
        .success()
        .stdout(predicate::str::contains("KIND"))
        .stdout(predicate::str::contains("WIN%"))
        .stdout(predicate::str::contains("P(SUCC)"))
        .stdout(predicate::str::contains("LAST USED"))
        .stdout(predicate::str::contains("implement"))
        .stdout(predicate::str::contains("sonnet5-claude"))
        .stdout(predicate::str::contains("100%"))
        .stdout(predicate::str::contains("6m12s"));

    // `--kind` filters, `--json` is machine readable.
    kevin(&url)
        .args(["routes", "--kind", "test"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no route scores yet"));
    let json = kevin(&url)
        .args(["--json", "routes", "--kind", "implement"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&json).expect("routes --json is JSON");
    assert_eq!(json["routes"].as_array().expect("routes array").len(), 2);
    assert_eq!(
        json["routes"][0]["alias"],
        serde_json::json!("sonnet5-claude")
    );
    assert_eq!(json["routes"][0]["stats"]["attempts"], serde_json::json!(4));
    assert_eq!(
        json["catalog_version"]
            .as_str()
            .expect("catalog version")
            .len(),
        64
    );

    // The leaderboard run also snapshotted the catalog into routing.model_aliases.
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM routing.model_aliases")
        .fetch_one(db.pool())
        .await
        .expect("catalog rows");
    assert_eq!(aliases, 8, "the default catalog has 8 aliases");

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_6_routes_explain_is_a_reproducible_dry_run() {
    kevin_testkit::skip_unless_pg!();
    let (db, _repo) = seeded_db().await;
    let url = db.url().to_owned();

    kevin(&url)
        .args([
            "routes",
            "explain",
            "--kind",
            "implement",
            "--complexity",
            "high",
            "--seed",
            "7",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ALIAS"))
        .stdout(predicate::str::contains("SAMPLED"))
        .stdout(predicate::str::contains("SCORE"))
        .stdout(predicate::str::contains("sonnet5-claude"))
        .stdout(predicate::str::contains("gpt56-codex"))
        .stdout(predicate::str::contains("opus5-claude"))
        .stdout(predicate::str::contains("policy: thompson"))
        .stdout(predicate::str::contains("route:"));

    // Explaining is a dry run: it records nothing.
    let outcomes: i64 = sqlx::query_scalar("SELECT count(*) FROM routing.route_outcomes")
        .fetch_one(db.pool())
        .await
        .expect("outcome rows");
    assert_eq!(outcomes, 5);

    // The same seed explains the same route; `--exclude` is honoured.
    let explain_json = |args: Vec<&str>| {
        let out = kevin(&url)
            .args(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<serde_json::Value>(&out).expect("explain --json is JSON")
    };
    let first = explain_json(vec![
        "--json",
        "routes",
        "explain",
        "--kind",
        "implement",
        "--seed",
        "3",
    ]);
    let second = explain_json(vec![
        "--json",
        "routes",
        "explain",
        "--kind",
        "implement",
        "--seed",
        "3",
    ]);
    assert_eq!(first, second, "a pinned seed is reproducible");
    let excluded = explain_json(vec![
        "--json",
        "routes",
        "explain",
        "--kind",
        "implement",
        "--seed",
        "3",
        "--exclude",
        "sonnet5-claude",
    ]);
    assert_ne!(
        excluded["route"]["model"],
        serde_json::json!("sonnet5-claude")
    );

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_6_routes_reset_restores_priors_and_emits_the_event() {
    kevin_testkit::skip_unless_pg!();
    let (db, repo) = seeded_db().await;
    let url = db.url().to_owned();

    kevin(&url)
        .args([
            "routes",
            "reset",
            "--kind",
            "implement",
            "--alias",
            "sonnet5-claude",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("reset implement / sonnet5-claude"));
    let stats = repo
        .stats_for(&TaskKind::Implement, &[alias("sonnet5-claude")])
        .await
        .expect("stats");
    assert_eq!(stats[&alias("sonnet5-claude")].attempts, 0);

    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM core.events WHERE event_type = 'routing.score_updated'",
    )
    .fetch_one(db.pool())
    .await
    .expect("event rows");
    assert_eq!(events, 1, "reset emits routing.score_updated");

    kevin(&url)
        .args(["routes", "reset", "--kind", "review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to reset"));

    db.close().await;
}
