//! Injectable command runner.
//!
//! Every subprocess this crate starts (`git`, `jj`, `gh`, repository check
//! commands) is described as a [`Cmd`] and executed through a
//! [`CommandRunner`]. Production uses [`ProcessRunner`] (`std::process`);
//! tests inject a recording/stubbing runner so that e.g. `gh pr create` is never
//! really invoked (`plan/12-workstreams.md` WS-07 acceptance 5).

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// A subprocess invocation: program, argv, working directory, extra env, stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// Program name or path (`git`, `jj`, `gh`, `sh`).
    pub program: String,
    /// Arguments (one argv entry each, never shell-joined).
    pub args: Vec<String>,
    /// Working directory; `None` inherits the parent's.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables set on top of the inherited environment.
    pub env: BTreeMap<String, String>,
    /// Data written to stdin (then closed). `None` closes stdin immediately.
    pub stdin: Option<String>,
}

impl Cmd {
    /// Builds a command with no cwd, env or stdin.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets the working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Sets one environment variable for the child.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Provides stdin content.
    #[must_use]
    pub fn stdin(mut self, data: impl Into<String>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// `program arg1 arg2 …` for logs and error messages.
    #[must_use]
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            if a.contains(char::is_whitespace) || a.is_empty() {
                let _ = write!(s, "{a:?}");
            } else {
                s.push_str(a);
            }
        }
        s
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Captured result of a finished command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    /// Exit code; `-1` when the process was killed by a signal.
    pub code: i32,
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
}

impl CmdOutput {
    /// A successful, silent output (handy for stubs).
    #[must_use]
    pub fn ok() -> Self {
        Self::default()
    }

    /// A successful output with the given stdout.
    pub fn ok_with(stdout: impl Into<String>) -> Self {
        Self {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed output with the given code and stderr.
    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// `true` when the exit code is zero.
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == 0
    }

    /// Last `max` bytes of stderr (falls back to stdout) for error messages.
    #[must_use]
    pub fn tail(&self, max: usize) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let text = text.trim_end();
        if text.len() <= max {
            return text.to_owned();
        }
        let mut start = text.len() - max;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        format!("…{}", &text[start..])
    }
}

/// Errors from running a command (the command ran but failed is *not* an
/// error here — see [`CmdOutput::success`]; callers decide).
#[derive(Debug, thiserror::Error)]
pub enum CmdError {
    /// The program could not be spawned (not found, permission, …).
    #[error("cannot spawn `{command}`: {source}")]
    Spawn {
        /// Rendered command line.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
    /// Reading/writing the child's pipes failed.
    #[error("io error while running `{command}`: {source}")]
    Io {
        /// Rendered command line.
        command: String,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },
}

/// Executes [`Cmd`]s. Implemented by [`ProcessRunner`] and by test doubles.
pub trait CommandRunner: Send + Sync + fmt::Debug {
    /// Runs the command to completion and captures its output.
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, CmdError>;
}

/// Runs commands with `std::process::Command`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, cmd: &Cmd) -> Result<CmdOutput, CmdError> {
        let mut command = std::process::Command::new(&cmd.program);
        command
            .args(&cmd.args)
            .envs(&cmd.env)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        tracing::debug!(command = %cmd, "spawn");
        let mut child = command.spawn().map_err(|source| CmdError::Spawn {
            command: cmd.display(),
            source,
        })?;
        if let Some(data) = &cmd.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            // A child that exits without reading stdin closes the pipe; that is
            // not an error for us.
            let _ = stdin.write_all(data.as_bytes());
            drop(stdin);
        }
        let output = child.wait_with_output().map_err(|source| CmdError::Io {
            command: cmd.display(),
            source,
        })?;
        let out = CmdOutput {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        tracing::debug!(command = %cmd, code = out.code, "exited");
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_arguments_with_whitespace() {
        let cmd = Cmd::new("gh")
            .args(["pr", "create", "--title"])
            .arg("Hello world");
        assert_eq!(cmd.display(), "gh pr create --title \"Hello world\"");
    }

    #[test]
    fn process_runner_captures_output_and_code() {
        let out = ProcessRunner
            .run(&Cmd::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]))
            .unwrap();
        assert_eq!(out.code, 3);
        assert_eq!(out.stdout.trim(), "out");
        assert_eq!(out.stderr.trim(), "err");
        assert!(!out.success());
    }

    #[test]
    fn process_runner_feeds_stdin() {
        let out = ProcessRunner.run(&Cmd::new("cat").stdin("ping")).unwrap();
        assert_eq!(out.stdout, "ping");
    }

    #[test]
    fn spawn_error_for_missing_binary() {
        let err = ProcessRunner
            .run(&Cmd::new("kevin-definitely-not-a-binary"))
            .unwrap_err();
        assert!(matches!(err, CmdError::Spawn { .. }));
    }

    #[test]
    fn tail_truncates_on_char_boundary() {
        let out = CmdOutput::failed(1, "ééééé");
        assert_eq!(out.tail(4), "…éé");
    }
}
