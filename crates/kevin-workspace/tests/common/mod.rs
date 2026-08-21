//! Shared helpers for the `kevin-workspace` integration tests: temp git/jj
//! repositories and a recording command runner that stubs `gh` and pushes.

#![allow(dead_code, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kevin_workspace::{Cmd, CmdError, CmdOutput, CommandRunner, ProcessRunner};
use tempfile::TempDir;

/// Runs `program args…` in `cwd`, panicking on failure; returns stdout.
pub fn sh(cwd: &Path, program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed ({}):\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Like [`sh`] but tolerates failure; returns `(success, stdout, stderr)`.
pub fn sh_try(cwd: &Path, program: &str, args: &[&str]) -> (bool, String, String) {
    match Command::new(program).args(args).current_dir(cwd).output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
    sh(cwd, "git", args)
}

pub fn jj(cwd: &Path, args: &[&str]) -> String {
    sh(cwd, "jj", args)
}

/// `true` when the `jj` binary is available.
pub fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A temp git repository on branch `main` with one commit (`README.md`, `shared.txt`).
pub fn git_repo() -> TempDir {
    let dir = tempfile::Builder::new()
        .prefix("kevin-ws-git-")
        .tempdir()
        .unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.name", "kevin-test"]);
    git(p, &["config", "user.email", "kevin-test@example.invalid"]);
    git(p, &["config", "commit.gpgsign", "false"]);
    std::fs::write(p.join("README.md"), "# fixture\n").unwrap();
    std::fs::write(p.join("shared.txt"), "line1\nline2\nline3\n").unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "init"]);
    dir
}

/// A temp jj repository (colocated with git when `colocate`), bookmark `main`
/// on one commit. `None` when `jj` is not installed (tests skip).
pub fn jj_repo(colocate: bool) -> Option<TempDir> {
    if !jj_available() {
        eprintln!("skipping: `jj` binary not found on PATH");
        return None;
    }
    let dir = tempfile::Builder::new()
        .prefix("kevin-ws-jj-")
        .tempdir()
        .unwrap();
    let p = dir.path();
    if colocate {
        git(p, &["init", "-q", "-b", "main"]);
        git(p, &["config", "user.name", "kevin-test"]);
        git(p, &["config", "user.email", "kevin-test@example.invalid"]);
        jj(p, &["git", "init", "--colocate"]);
    } else {
        jj(p, &["git", "init", "--no-colocate"]);
    }
    jj(p, &["config", "set", "--repo", "user.name", "kevin-test"]);
    jj(
        p,
        &[
            "config",
            "set",
            "--repo",
            "user.email",
            "kevin-test@example.invalid",
        ],
    );
    std::fs::write(p.join("README.md"), "# fixture\n").unwrap();
    std::fs::write(p.join("shared.txt"), "line1\nline2\nline3\n").unwrap();
    jj(p, &["commit", "-m", "init"]);
    jj(p, &["bookmark", "create", "main", "-r", "@-"]);
    Some(dir)
}

/// Writes `content` to `file` inside a workspace and commits it (git or jj).
pub fn commit_in(ws_root: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(ws_root.join(file), content).unwrap();
    if ws_root.join(".jj").exists() {
        jj(ws_root, &["commit", "-m", message]);
    } else {
        git(ws_root, &["add", "-A"]);
        git(ws_root, &["commit", "-q", "-m", message]);
    }
}

/// A stub: when `program` matches and every `args_prefix` entry is found in the
/// argv (in order, contiguous from the start), `output` is returned instead of
/// running the command.
#[derive(Debug, Clone)]
pub struct Stub {
    pub program: String,
    pub args_prefix: Vec<String>,
    pub output: CmdOutput,
}

/// Records every command; runs them for real except stubbed ones.
#[derive(Debug, Default)]
pub struct RecordingRunner {
    pub calls: Mutex<Vec<Cmd>>,
    pub stubs: Mutex<Vec<Stub>>,
}

impl RecordingRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Stubs `program args_prefix…` with `output`.
    pub fn stub(
        self: &Arc<Self>,
        program: &str,
        args_prefix: &[&str],
        output: CmdOutput,
    ) -> Arc<Self> {
        self.stubs.lock().unwrap().push(Stub {
            program: program.to_owned(),
            args_prefix: args_prefix.iter().map(|s| (*s).to_owned()).collect(),
            output,
        });
        Arc::clone(self)
    }

    /// Stubs `gh` (any args) returning `url` and `git push` / `jj git push` as success.
    pub fn stub_remote(self: &Arc<Self>, url: &str) -> Arc<Self> {
        self.stub("gh", &[], CmdOutput::ok_with(format!("{url}\n")));
        self.stub("git", &["push"], CmdOutput::ok());
        self.stub("jj", &["git", "push"], CmdOutput::ok());
        Arc::clone(self)
    }

    /// Recorded calls of `program` whose argv starts with `args_prefix`.
    pub fn calls_of(&self, program: &str, args_prefix: &[&str]) -> Vec<Cmd> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.program == program && starts_with(&c.args, args_prefix))
            .cloned()
            .collect()
    }
}

fn starts_with(args: &[String], prefix: &[&str]) -> bool {
    prefix.len() <= args.len() && prefix.iter().zip(args).all(|(p, a)| p == a)
}

impl CommandRunner for RecordingRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, CmdError> {
        self.calls.lock().unwrap().push(cmd.clone());
        let stub = self
            .stubs
            .lock()
            .unwrap()
            .iter()
            .find(|s| {
                s.program == cmd.program
                    && s.args_prefix.len() <= cmd.args.len()
                    && s.args_prefix.iter().zip(&cmd.args).all(|(p, a)| p == a)
            })
            .cloned();
        match stub {
            Some(s) => Ok(s.output),
            None => ProcessRunner.run(cmd),
        }
    }
}

/// Value of the argument following `flag` in `args`.
pub fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

pub fn path_of(dir: &TempDir) -> PathBuf {
    dir.path().canonicalize().unwrap()
}
