//! WS-05 acceptance (5): `kevin workers doctor` reports missing binaries
//! without panicking. `PATH` is pointed at an empty directory so no real
//! coding-agent CLI can be found (and none is ever invoked).

use assert_cmd::Command;
use predicates::prelude::*;

fn kevin(path_dir: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("kevin").expect("kevin binary");
    cmd.env_clear()
        .env("PATH", path_dir)
        .env("HOME", path_dir)
        .env("NO_COLOR", "1");
    cmd
}

#[test]
fn ac_ws05_5_workers_doctor_reports_missing_binaries_without_panicking() {
    let empty = tempfile::tempdir().unwrap();
    let assert = kevin(empty.path()).args(["workers", "doctor"]).assert();
    assert
        .code(1)
        .stdout(predicate::str::contains("KIND"))
        .stdout(predicate::str::contains("claude"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("pi"))
        .stdout(predicate::str::contains("opencode"))
        .stdout(predicate::str::contains(
            "missing (workers.claude.bin = \"claude\")",
        ))
        .stdout(predicate::str::contains("fake"))
        .stderr(predicate::str::contains("panicked").not())
        .stderr(predicate::str::contains("unhealthy"));
}

#[test]
fn workers_doctor_json_is_machine_readable() {
    let empty = tempfile::tempdir().unwrap();
    let output = kevin(empty.path())
        .args(["--json", "workers", "doctor"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["healthy"], false);
    let workers = value["workers"].as_array().unwrap();
    assert_eq!(workers.len(), 5);
    assert!(workers.iter().all(|w| w["binary"].is_string()));
}

#[test]
fn workers_ls_lists_every_kind_and_exits_zero() {
    let empty = tempfile::tempdir().unwrap();
    kevin(empty.path())
        .args(["workers", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("opus5-claude"))
        .stdout(predicate::str::contains("fake"));
}
