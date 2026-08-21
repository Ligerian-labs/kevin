//! WS-03 acceptance criterion (5), CLI half: `kevin db init|migrate|status|reset`
//! work against a real Postgres and migrations are idempotent.

use std::process::Command;

use assert_cmd::prelude::*;
use kevin_testkit::pg::{TestDb, admin_url, drop_database, with_database};
use predicates::prelude::*;

fn kevin() -> Command {
    let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
    // Never let the ambient environment pick the database for these tests.
    cmd.env_remove("KEVIN__DATABASE__URL")
        .env_remove("DATABASE_URL");
    cmd
}

#[tokio::test]
async fn ac_ws03_5_db_commands_and_idempotent_migrations() {
    kevin_testkit::skip_unless_pg!();
    let admin = admin_url();
    let fresh = format!("kevin_test_{}_cli", uuid::Uuid::now_v7().simple());
    let fresh = &fresh[..fresh.len().min(60)];
    let fresh_url = with_database(&admin, fresh);
    let _ = drop_database(&admin, fresh).await;

    // status on a database that does not exist yet: unreachable/failed, non-zero.
    kevin()
        .args(["db", "--url", &fresh_url, "status"])
        .assert()
        .failure();

    // init creates the database (admin connection = DATABASE_URL) + extension + migrates.
    kevin()
        .args(["db", "--url", &fresh_url, "init", "--admin-url", &admin])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("database created").or(predicate::str::contains("created")),
        )
        .stdout(predicate::str::contains("migrations: applied [1]"));

    // status: current, pgvector installed, exit 0.
    kevin()
        .args(["db", "--url", &fresh_url, "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pgvector: installed"))
        .stdout(predicate::str::contains("status: current"));
    let json = kevin()
        .args(["--json", "db", "--url", &fresh_url, "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&json).expect("status --json is JSON");
    assert_eq!(json["current"], serde_json::json!(true));
    assert_eq!(json["pending"], serde_json::json!([]));
    assert!(json["database"].as_str().unwrap().contains("***") || !admin.contains(':'));

    // migrate twice: idempotent (second applies nothing).
    kevin()
        .args(["db", "--url", &fresh_url, "migrate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to apply"));
    kevin()
        .args(["db", "--url", &fresh_url, "init", "--admin-url", &admin])
        .assert()
        .success()
        .stdout(predicate::str::contains("already present"))
        .stdout(predicate::str::contains("nothing to apply"));

    // reset refuses without --yes and outside the laptop profile; works with both.
    kevin()
        .args(["db", "--url", &fresh_url, "reset"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("--yes"));
    kevin()
        .args([
            "--set",
            "kevin.profile=server",
            "db",
            "--url",
            &fresh_url,
            "reset",
            "--yes",
        ])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("laptop"));
    kevin()
        .args(["db", "--url", &fresh_url, "reset", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("applied [1]"));
    kevin()
        .args(["db", "--url", &fresh_url, "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("status: current"));

    // prune runs; rebuild-projection is an explicit stub until WS-11.
    kevin()
        .args(["db", "--url", &fresh_url, "prune"])
        .assert()
        .success()
        .stdout(predicate::str::contains("pruned 0 delivered outbox rows"));
    kevin()
        .args(["db", "--url", &fresh_url, "rebuild-projection", "--all"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("WS-11"));

    // `--set database.url=` and a TestDb work too (no --url).
    let db = TestDb::new().await;
    kevin()
        .args([
            "--set",
            &format!("database.url={}", db.url()),
            "db",
            "status",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("status: current"));
    db.close().await;

    drop_database(&admin, fresh)
        .await
        .expect("drop cli test db");
}
