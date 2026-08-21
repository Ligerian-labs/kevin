//! Postgres-backed half of WS-09: the `routing` schema round-trips
//! (`plan/06-memory-and-learning.md` §2.1) and the Postgres repository behaves
//! exactly like the in-memory one. One database per test
//! (`kevin_testkit::pg::TestDb`).

use std::sync::Arc;

use chrono::Utc;
use kevin_config::KevinConfig;
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome, ResetRouteScore};
use kevin_domain::{Decimal, FailureClass, ModelAlias, RouteStats, TaskKind};
use kevin_router::{
    AttemptRef, CatalogRepo, ModelCatalog, PgRouteScoreRepo, RouteScoreRepo, Router,
    SeededRngSource, SelectRouteQuery,
};
use kevin_testkit::pg::TestDb;
use rust_decimal::prelude::FromPrimitive;
use uuid::Uuid;

fn alias(name: &str) -> ModelAlias {
    ModelAlias::new(name).expect("valid alias")
}

fn outcome(alias_name: &str, success: bool) -> RecordRouteOutcome {
    RecordRouteOutcome {
        task_kind: TaskKind::Implement,
        alias: alias(alias_name),
        success,
        quality: Some(if success { 0.8 } else { 0.3 }),
        cost_usd: Decimal::from_f64(0.25),
        wall_ms: 120_000,
        failure_class: (!success).then_some(FailureClass::Permanent),
        recorded_at: Utc::now(),
        prior: BetaPrior::UNIFORM,
    }
}

#[tokio::test]
async fn ac_ws09_pg_catalog_snapshot_is_versioned_and_idempotent() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let config = KevinConfig::default();
    let catalog = ModelCatalog::from_config(&config);
    let repo = CatalogRepo::new(db.pool().clone());

    let first = repo.sync(&catalog).await.expect("sync");
    assert!(!first.already_present);
    assert_eq!(first.aliases, catalog.len());
    assert_eq!(first.rows_written, catalog.len());
    assert_eq!(first.catalog_version, catalog.version());

    let second = repo.sync(&catalog).await.expect("re-sync");
    assert!(second.already_present, "the version was already stored");
    assert_eq!(
        repo.aliases_of(catalog.version())
            .await
            .expect("aliases")
            .len(),
        catalog.len()
    );

    // Editing the catalog produces a new version, side by side with the old one.
    let mut edited = config.clone();
    edited
        .models
        .get_mut(&alias("sonnet5-claude"))
        .expect("alias")
        .tags
        .push("golden".to_owned());
    let edited_catalog = ModelCatalog::from_config(&edited);
    assert_ne!(edited_catalog.version(), catalog.version());
    repo.sync(&edited_catalog).await.expect("sync edited");
    assert_eq!(repo.versions().await.expect("versions").len(), 2);

    // Prices and tags survive the round trip.
    let row: (Option<Decimal>, Option<Decimal>, Vec<String>, String) = sqlx::query_as(
        "SELECT input_usd_per_m, output_usd_per_m, tags, tier FROM routing.model_aliases \
         WHERE catalog_version = $1 AND alias = 'sonnet5-claude'",
    )
    .bind(catalog.version())
    .fetch_one(db.pool())
    .await
    .expect("row");
    assert_eq!(row.0, Some(Decimal::from(3)));
    assert_eq!(row.1, Some(Decimal::from(15)));
    assert!(row.2.contains(&"coding".to_owned()));
    assert_eq!(row.3, "balanced");

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_pg_scores_and_outcomes_round_trip() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let repo = PgRouteScoreRepo::new(db.pool().clone());
    let version = "cat-v1";

    let updated = repo
        .record(&outcome("sonnet5-claude", true), None, version)
        .await
        .expect("record")
        .expect("first outcome");
    assert_eq!(updated.stats.attempts, 1);
    assert_eq!(updated.version, 1);

    let stats = repo
        .stats_for(&TaskKind::Implement, &[alias("sonnet5-claude")])
        .await
        .expect("stats");
    let stored = stats.get(&alias("sonnet5-claude")).expect("row");
    // `timestamptz` keeps microseconds, the in-memory value may carry
    // nanoseconds: compare the timestamp with microsecond tolerance and the
    // rest of the statistics exactly.
    let (stored_ts, memory_ts) = (
        stored.last_used.expect("stored last_used"),
        updated.stats.last_used.expect("in-memory last_used"),
    );
    assert!(
        (memory_ts - stored_ts)
            .num_microseconds()
            .is_some_and(|us| us.abs() <= 1),
        "last_used differs by more than a microsecond: {memory_ts} vs {stored_ts}"
    );
    let expected = RouteStats {
        last_used: stored.last_used,
        ..updated.stats.clone()
    };
    assert_eq!(stored, &expected);
    assert_eq!(stored.mean_cost_usd(), Decimal::from_f64(0.25));
    assert_eq!(stored.mean_wall_ms(), Some(120_000));

    // The generated columns agree with the Rust-side means.
    let (mean_cost, mean_wall): (Option<Decimal>, Option<i64>) = sqlx::query_as(
        "SELECT mean_cost_usd, mean_wall_ms FROM routing.route_scores \
         WHERE task_kind = 'implement' AND alias = 'sonnet5-claude'",
    )
    .fetch_one(db.pool())
    .await
    .expect("row");
    assert_eq!(mean_cost, Decimal::from_f64(0.25));
    assert_eq!(mean_wall, Some(120_000));

    // The outcome row carries the catalog version.
    let (rows, catalog_version): (i64, String) =
        sqlx::query_as("SELECT count(*), min(catalog_version) FROM routing.route_outcomes")
            .fetch_one(db.pool())
            .await
            .expect("row");
    assert_eq!(rows, 1);
    assert_eq!(catalog_version, version);

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_pg_outcomes_are_idempotent_per_attempt() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let repo = PgRouteScoreRepo::new(db.pool().clone());
    let attempt = AttemptRef::new(Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

    assert!(
        repo.record(&outcome("sonnet5-claude", true), Some(attempt), "v1")
            .await
            .expect("record")
            .is_some()
    );
    // A re-evaluation of the same attempt refreshes the row but not the score.
    let mut rejudged = outcome("sonnet5-claude", true);
    rejudged.quality = Some(0.4);
    assert!(
        repo.record(&rejudged, Some(attempt), "v1")
            .await
            .expect("record")
            .is_none()
    );

    let stats = repo
        .stats_for(&TaskKind::Implement, &[alias("sonnet5-claude")])
        .await
        .expect("stats");
    assert_eq!(stats[&alias("sonnet5-claude")].attempts, 1);

    let (rows, quality): (i64, Option<f32>) =
        sqlx::query_as("SELECT count(*), min(quality) FROM routing.route_outcomes")
            .fetch_one(db.pool())
            .await
            .expect("row");
    assert_eq!(rows, 1, "one row per attempt");
    assert!(
        quality.is_some_and(|q| (q - 0.4).abs() < 1e-6),
        "the row keeps the latest judgement: {quality:?}"
    );

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_pg_leaderboard_view_and_reset() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let repo = PgRouteScoreRepo::new(db.pool().clone());

    for _ in 0..3 {
        repo.record(&outcome("sonnet5-claude", true), None, "v1")
            .await
            .expect("record");
    }
    repo.record(&outcome("gpt56-codex", false), None, "v1")
        .await
        .expect("record");

    let rows = repo.leaderboard(None).await.expect("leaderboard");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].alias, alias("sonnet5-claude"));
    assert_eq!(rows[0].stats.attempts, 3);
    assert!((rows[0].stats.win_rate() - 1.0).abs() < f32::EPSILON);

    let (win_rate, p_success): (f32, f32) = sqlx::query_as(
        "SELECT win_rate, p_success FROM routing.route_leaderboard \
         WHERE task_kind = 'implement' AND alias = 'sonnet5-claude'",
    )
    .fetch_one(db.pool())
    .await
    .expect("view row");
    assert!((win_rate - 1.0).abs() < f32::EPSILON);
    assert!((p_success - rows[0].stats.p_success()).abs() < 1e-6);

    let filtered = repo
        .leaderboard(Some(&TaskKind::Test))
        .await
        .expect("leaderboard");
    assert!(filtered.is_empty());

    let reset = ResetRouteScore {
        task_kind: TaskKind::Implement,
        alias: alias("sonnet5-claude"),
        prior: BetaPrior::for_tier(kevin_domain::Tier::Balanced),
    };
    let updated = repo.reset(&reset).await.expect("reset").expect("row");
    assert!(updated.reset);
    assert_eq!(updated.stats.attempts, 0);
    assert!((updated.stats.alpha - 2.0).abs() < f32::EPSILON);
    assert!(updated.version >= 4, "version keeps increasing");
    let missing = ResetRouteScore {
        task_kind: TaskKind::Review,
        ..reset
    };
    assert!(repo.reset(&missing).await.expect("reset").is_none());

    db.close().await;
}

#[tokio::test]
async fn ac_ws09_pg_router_learns_through_postgres() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let mut config = KevinConfig::default();
    // Two candidates only: no unsampled third alias, so the assertion measures
    // learning rather than the exploration floor (that is `ac_ws09_3`).
    config
        .routing
        .kinds
        .get_mut(&TaskKind::Implement)
        .expect("implement candidates")
        .candidates = vec![alias("sonnet5-claude"), alias("opus5-claude")];
    let repo = Arc::new(PgRouteScoreRepo::new(db.pool().clone()));
    let router = Router::from_config(&config, repo).with_rng(Arc::new(SeededRngSource::new(2024)));

    CatalogRepo::new(db.pool().clone())
        .sync(router.catalog())
        .await
        .expect("catalog sync");

    for index in 0..30 {
        let run = Uuid::now_v7();
        router
            .record_attempt_outcome(
                outcome("opus5-claude", true),
                Some(AttemptRef::new(run, Uuid::now_v7(), Uuid::now_v7())),
            )
            .await
            .expect("record winner");
        let mut loser = outcome("sonnet5-claude", false);
        loser.wall_ms = 300_000 + index;
        router
            .record_attempt_outcome(
                loser,
                Some(AttemptRef::new(run, Uuid::now_v7(), Uuid::now_v7())),
            )
            .await
            .expect("record loser");
    }

    let mut winner = 0;
    for _ in 0..40 {
        let selection = router
            .select(SelectRouteQuery::new(TaskKind::Implement))
            .await
            .expect("route");
        assert_eq!(selection.catalog_version, router.catalog().version());
        if selection.alias() == &alias("opus5-claude") {
            winner += 1;
        }
    }
    assert!(winner >= 32, "learned winner selected {winner}/40 times");

    let rows = router
        .leaderboard(Some(&TaskKind::Implement))
        .await
        .expect("leaderboard");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].alias, alias("opus5-claude"));

    db.close().await;
}
