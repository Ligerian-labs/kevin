//! WS-17 at the CLI level: `kevin tui` resolves its daemon (`plan/07-api-and-tui.md`
//! §3–4). Every invocation runs with `HOME`/`XDG_CONFIG_HOME` in a temp dir so
//! nothing reads the real `~/.config/kevin`.
//!
//! The interactive session itself is covered by `kevin-tui`'s own acceptance
//! tests (reducer + `TestBackend` + fake API); here we only pin what the CLI
//! does *before* it takes over the terminal.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            _dir: dir,
            home,
            cwd,
        }
    }

    fn kevin(&self) -> Command {
        let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .current_dir(&self.cwd);
        cmd
    }

    fn token_file(&self) -> PathBuf {
        self.home.join("token")
    }

    fn write_token(&self) -> PathBuf {
        let path = self.token_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "test-token").unwrap();
        path
    }
}

#[test]
fn ac_ws17_tui_without_a_server_explains_that_kevin_serve_is_needed() {
    let sandbox = Sandbox::new();
    sandbox
        .kevin()
        .arg("tui")
        .assert()
        .failure()
        .code(3)
        .stderr(
            predicate::str::contains("no Kevin daemon configured")
                .and(predicate::str::contains("kevin serve"))
                .and(predicate::str::contains("--server")),
        );
}

#[test]
fn ac_ws17_tui_with_a_server_but_no_token_file_says_which_path_failed() {
    let sandbox = Sandbox::new();
    let missing = sandbox.home.join("nope/token");
    sandbox
        .kevin()
        .args(["tui", "--server", "http://127.0.0.1:7777"])
        .args(["--token-file", missing.to_str().unwrap()])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains(missing.display().to_string()));
}

#[test]
fn ac_ws17_tui_reports_an_unreachable_daemon_instead_of_opening_a_terminal() {
    let sandbox = Sandbox::new();
    let token = sandbox.write_token();
    // Bind and drop so the port is free but nothing answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    sandbox
        .kevin()
        .args(["tui", "--server", &format!("http://{addr}")])
        .args(["--token-file", token.to_str().unwrap()])
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains(addr.to_string()));
}

#[test]
fn ac_ws17_tui_rejects_a_malformed_server_url() {
    let sandbox = Sandbox::new();
    let token = sandbox.write_token();
    sandbox
        .kevin()
        .args(["tui", "--server", "not a url"])
        .args(["--token-file", token.to_str().unwrap()])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("invalid URL"));
}

#[test]
fn ac_ws17_tui_help_documents_the_run_flag() {
    let sandbox = Sandbox::new();
    sandbox
        .kevin()
        .args(["tui", "--help"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("--run")
                .and(predicate::str::contains("--server"))
                .and(predicate::str::contains("Open the terminal UI")),
        );
}

#[test]
fn ac_ws17_tui_reads_the_server_from_the_environment() {
    let sandbox = Sandbox::new();
    let token = sandbox.write_token();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    // `KEVIN__CLIENT__SERVER_URL` is the env layer of `client.server_url`, so
    // the command must get past "no daemon configured" and fail on the socket.
    sandbox
        .kevin()
        .env("KEVIN__CLIENT__SERVER_URL", format!("http://{addr}"))
        .env("KEVIN__CLIENT__TOKEN_FILE", token.to_str().unwrap())
        .arg("tui")
        .assert()
        .failure()
        .code(4)
        .stderr(predicate::str::contains(addr.to_string()));
}
