//! WS-00 acceptance criteria (`plan/12-workstreams.md` §WS-00), automated
//! where a test can check them; `just ci` / CI check the rest.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

/// Every top-level command from `plan/07-api-and-tui.md` §3.
const COMMANDS: &[&str] = &[
    "run",
    "serve",
    "tui",
    "runs",
    "tasks",
    "questions",
    "answer",
    "approve",
    "reject",
    "db",
    "config",
    "workers",
    "routes",
    "lessons",
    "memory",
    "eval",
    "proposals",
    "cost",
    "kohral",
    "completions",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn kevin() -> Command {
    Command::cargo_bin("kevin").expect("kevin binary is built")
}

#[test]
fn ac_ws00_1_just_ci_is_fmt_clippy_deny_nextest() {
    // `just ci` itself runs in CI; here we pin the recipe composition.
    let justfile = read("justfile");
    let ci_line = justfile
        .lines()
        .find(|l| l.starts_with("ci:"))
        .expect("justfile has a `ci` recipe");
    for dep in ["fmt-check", "clippy", "deny", "test"] {
        assert!(
            ci_line.contains(dep),
            "`ci` recipe must depend on `{dep}`: {ci_line}"
        );
    }
    assert!(justfile.contains("cargo fmt --all -- --check"));
    assert!(justfile.contains("-- -D warnings"));
    assert!(justfile.contains("cargo deny check"));
    assert!(justfile.contains("cargo nextest run"));
    assert!(justfile.contains("postgres://kevin:kevin@localhost:5433/kevin"));
    let workflow = read(".github/workflows/ci.yml");
    assert!(workflow.contains("pgvector/pgvector:pg16"));
    assert!(workflow.contains("cargo deny check"));
    assert!(workflow.contains("cargo nextest run"));
    assert!(workflow.contains("macos-latest"));
}

#[test]
fn ac_ws00_2_help_lists_every_command() {
    let assert = kevin().arg("--help").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for name in COMMANDS {
        let listed = stdout.lines().any(|l| l.trim_start().starts_with(name));
        assert!(listed, "`kevin --help` must list `{name}`:\n{stdout}");
    }
}

#[test]
fn ac_ws00_2_every_command_has_help() {
    for name in COMMANDS {
        kevin()
            .args([name, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Usage"));
    }
}

#[test]
fn ac_ws00_2_stubs_exit_2_not_implemented() {
    let cases: &[&[&str]] = &[
        &["serve"],
        // `db` (WS-03), `config show` (WS-02), `workers doctor` (WS-05),
        // `routes` (WS-09), `lessons`/`memory` (WS-18), `proposals` (WS-19),
        // `tui` (WS-17) and `run`/`runs`/`tasks`/`questions`/`answer`/`approve`/
        // `reject`/`cost` (WS-12) are implemented.
        &["eval", "rerun", "01910000-0000-7000-8000-000000000001"],
        &["kohral", "conformance", "--phase", "basic"],
    ];
    for args in cases {
        kevin()
            .args(*args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("not implemented yet"));
    }
}

#[test]
fn ac_ws00_2_invalid_usage_exits_3() {
    kevin().arg("nope").assert().code(3);
    kevin()
        .args(["runs", "show", "not-a-uuid"])
        .assert()
        .code(3);
    kevin().args(["routes", "--kind", "Bad"]).assert().code(3);
    kevin().args(["memory", "forget"]).assert().code(3);
    kevin().args(["db", "rebuild-projection"]).assert().code(3);
}

#[test]
fn ac_ws00_2_completions_are_generated() {
    kevin()
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kevin"));
    kevin().args(["completions", "zsh"]).assert().success();
    kevin().args(["completions", "fish"]).assert().success();
}

#[test]
fn ac_ws00_2_global_flags_parse_anywhere() {
    kevin()
        .args([
            "--json", "-vv", "--set", "a.b=c", "runs", "ls", "--server", "http://x",
        ])
        .assert()
        .code(2);
    kevin().args(["-v", "-q", "runs", "ls"]).assert().code(3);
}

#[test]
fn ac_ws00_3_deny_toml_has_initial_license_allowlist() {
    let deny = read("deny.toml");
    for license in [
        "MIT",
        "Apache-2.0",
        "BSD-2-Clause",
        "BSD-3-Clause",
        "ISC",
        "Unicode-3.0",
    ] {
        assert!(
            deny.contains(&format!("\"{license}\"")),
            "deny.toml must allow {license}"
        );
    }
    assert!(deny.contains("[advisories]") && deny.contains("[bans]") && deny.contains("[sources]"));
}

#[test]
fn ac_ws00_4_claude_md_at_most_15_lines_and_mirrored_in_agents_md() {
    let claude = read("CLAUDE.md");
    let agents = read("AGENTS.md");
    assert!(
        claude.lines().count() <= 15,
        "CLAUDE.md has {} lines (> 15)",
        claude.lines().count()
    );
    assert_eq!(claude, agents, "AGENTS.md must be identical to CLAUDE.md");
    assert!(claude.contains("plan/"));
    assert!(claude.contains("just ci"));
}

#[test]
fn ac_ws00_5_compose_file_defines_pgvector() {
    let compose = read("deploy/compose/postgres.yml");
    assert!(compose.contains("pgvector/pgvector:pg16"));
    assert!(compose.contains("POSTGRES_USER: kevin"));
    assert!(compose.contains("POSTGRES_PASSWORD: kevin"));
    assert!(compose.contains("POSTGRES_DB: kevin"));
    assert!(compose.contains(":5432\""));
    assert!(compose.contains("volumes:"));
}

#[test]
fn ac_ws00_6_workspace_lists_every_crate_from_the_crate_map() {
    let crates = repo_root().join("crates");
    for name in [
        "kevin-domain",
        "kevin-config",
        "kevin-store",
        "kevin-bus",
        "kevin-telemetry",
        "kevin-workspace",
        "kevin-worker",
        "kevin-router",
        "kevin-memory",
        "kevin-evaluator",
        "kevin-orchestrator",
        "kevin-api",
        "kevin-kohral",
        "kevin-tui",
        "kevin-cli",
        "kevin-testkit",
    ] {
        assert!(
            crates.join(name).join("Cargo.toml").is_file(),
            "missing crate {name}"
        );
        let src = std::fs::read_to_string(crates.join(name).join("src/lib.rs")).unwrap();
        assert!(
            src.starts_with("//!"),
            "{name}/src/lib.rs must start with a crate-level doc comment"
        );
    }
}
