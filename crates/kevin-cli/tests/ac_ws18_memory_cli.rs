//! WS-18, CLI half: `kevin memory add|search|forget|doctor|export|import` and
//! `kevin lessons` against a real Postgres (`plan/06-memory-and-learning.md`
//! §1.7). The embedder is `none`, so no model is downloaded.

use std::process::Command;

use assert_cmd::prelude::*;
use kevin_testkit::pg::TestDb;
use predicates::prelude::*;

fn kevin(url: &str) -> Command {
    let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
    cmd.env_remove("KEVIN__DATABASE__URL")
        .env_remove("DATABASE_URL")
        .arg("--set")
        .arg(format!("database.url={url}"))
        .arg("--set")
        .arg("memory.embedder=none");
    cmd
}

#[tokio::test]
async fn ac_ws18_7_memory_cli_round_trip() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let url = db.url().to_owned();

    // add
    kevin(&url)
        .args([
            "memory",
            "add",
            "--kind",
            "fact",
            "Kevin stores no provider API keys",
            "--tag",
            "security",
            "--global",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("stored"));

    // add refuses secrets (acceptance criterion 4, through the CLI)
    kevin(&url)
        .args([
            "memory",
            "add",
            "--kind",
            "fact",
            "the key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
            "--global",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("contains"));

    // search --json
    let out = kevin(&url)
        .args(["--json", "memory", "search", "provider API keys"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("search --json is JSON");
    let hits = json["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "{json}");
    assert_eq!(hits[0]["kind"], "fact");
    assert_eq!(hits[0]["scope"], "global");
    let id = hits[0]["id"].as_str().expect("id").to_owned();

    // doctor
    kevin(&url)
        .args(["memory", "doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hnsw index:          present"))
        .stdout(predicate::str::contains("items (live):        1"));

    // export → import into another database
    let export = kevin(&url)
        .args(["memory", "export", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let items: serde_json::Value = serde_json::from_slice(&export).expect("export is JSON");
    assert_eq!(items.as_array().map(Vec::len), Some(1));
    let file = tempfile::NamedTempFile::new().expect("temp file");
    std::fs::write(file.path(), &export).expect("write export");

    let other = TestDb::new().await;
    kevin(other.url())
        .args(["memory", "import", &file.path().to_string_lossy()])
        .assert()
        .success()
        .stdout(predicate::str::contains("imported 1"));
    other.close().await;

    // lessons: none yet, and the fact is not one
    kevin(&url)
        .args(["lessons"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no lessons yet"));

    // forget
    kevin(&url)
        .args(["memory", "forget", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("forgot 1"));
    let out = kevin(&url)
        .args(["--json", "memory", "search", "provider API keys"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).expect("search --json is JSON");
    assert!(json["hits"].as_array().expect("hits").is_empty());

    // reindex without an embedder is refused, not a silent no-op
    kevin(&url)
        .args(["memory", "reindex"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("embeddings are disabled"));

    db.close().await;
}
