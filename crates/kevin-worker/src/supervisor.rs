//! Subprocess supervisor shared by the CLI adapters (`plan/04-workers.md`
//! §Subprocess supervisor, `plan/01-architecture.md` §Process model).
//!
//! - `tokio::process::Command`, `kill_on_drop(true)`, own process group
//!   (`process_group(0)`) so the whole tree can be signalled, `cwd` =
//!   workspace root, `env_clear()` then only the allow-listed variables.
//! - stdin: payload written then closed.
//! - stdout/stderr: bounded line readers (line length capped, longer lines
//!   truncated and counted) feeding a bounded channel; when the consumer lags
//!   the reader awaits and the child blocks on its pipe — memory is bounded by
//!   construction.
//! - Timeout / cancellation: `SIGTERM` to the process group, wait
//!   `kill_grace`, then `SIGKILL`.
//! - Transcript: every raw line (stdout and stderr, tagged) appended as JSONL to
//!   the configured path; an `ArtifactRef{kind: Transcript}` is returned.
//! - Exit classification table → [`classify`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use kevin_domain::{FailureClass, WorkerKind};
use kevin_telemetry::metrics as metric_names;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::types::{ArtifactKind, ArtifactRef};
use crate::worker::WorkerError;

/// Default cap on one output line (longer lines are truncated).
pub const DEFAULT_MAX_LINE_BYTES: usize = 1024 * 1024;
/// Default capacity of the line channel (lines, not bytes).
pub const DEFAULT_LINE_CAPACITY: usize = 256;
/// How much of stderr is kept for `Failed.message`.
pub const STDERR_TAIL_BYTES: usize = 4096;
/// `workers.kill_grace` default.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(10);
/// Read buffer of each pipe reader.
const READ_BUFFER_BYTES: usize = 64 * 1024;
/// Capacity of the transcript channel (records).
const TRANSCRIPT_CAPACITY: usize = 1024;

/// Which pipe a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl Stream {
    const fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// One line of child output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    /// Pipe.
    pub stream: Stream,
    /// Line without the trailing newline (lossy UTF-8).
    pub text: String,
    /// `true` when the line exceeded `max_line_bytes` and was cut.
    pub truncated: bool,
}

/// Spawn options.
#[derive(Debug)]
pub struct SpawnOpts {
    /// Worker kind (metrics/log label).
    pub kind: WorkerKind,
    /// Working directory (workspace root).
    pub cwd: PathBuf,
    /// Complete environment of the child (allow-listed values + `KEVIN_*`).
    pub env: BTreeMap<String, String>,
    /// Payload written to stdin, then stdin is closed. `None` → stdin is `/dev/null`.
    pub stdin: Option<Vec<u8>>,
    /// `SIGTERM` → `SIGKILL` delay.
    pub kill_grace: Duration,
    /// Wall-clock timeout; expiry kills the tree and reports [`ExitReason::Timeout`].
    pub timeout: Option<Duration>,
    /// Cancelling it kills the tree and reports [`ExitReason::Cancelled`].
    pub cancel: CancellationToken,
    /// Where to append the JSONL transcript; `None` → no transcript.
    pub transcript: Option<PathBuf>,
    /// Cap on one line.
    pub max_line_bytes: usize,
    /// Capacity of the line channel.
    pub line_capacity: usize,
}

impl SpawnOpts {
    /// Defaults for `kind` in `cwd`: no stdin, 10 s grace, no timeout, fresh
    /// token, no transcript, 1 MiB lines, 256-line channel.
    pub fn new(kind: WorkerKind, cwd: impl Into<PathBuf>) -> Self {
        Self {
            kind,
            cwd: cwd.into(),
            env: BTreeMap::new(),
            stdin: None,
            kill_grace: DEFAULT_KILL_GRACE,
            timeout: None,
            cancel: CancellationToken::new(),
            transcript: None,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            line_capacity: DEFAULT_LINE_CAPACITY,
        }
    }

    /// Sets the child environment.
    #[must_use]
    pub fn env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    /// Sets the stdin payload.
    #[must_use]
    pub fn stdin(mut self, payload: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(payload.into());
        self
    }

    /// Sets the kill grace.
    #[must_use]
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Sets the timeout.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the cancellation token.
    #[must_use]
    pub fn cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets the transcript path.
    #[must_use]
    pub fn transcript(mut self, path: impl Into<PathBuf>) -> Self {
        self.transcript = Some(path.into());
        self
    }

    /// Sets the line cap.
    #[must_use]
    pub fn max_line_bytes(mut self, max: usize) -> Self {
        self.max_line_bytes = max.max(1);
        self
    }

    /// Sets the line channel capacity.
    #[must_use]
    pub fn line_capacity(mut self, capacity: usize) -> Self {
        self.line_capacity = capacity.max(1);
        self
    }
}

/// Why the child stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "value", rename_all = "snake_case")]
pub enum ExitReason {
    /// Exited by itself with this code.
    Exited(i32),
    /// Killed by this signal (not by us).
    Signaled(i32),
    /// Killed by us after the cancellation token fired.
    Cancelled,
    /// Killed by us after the timeout expired.
    Timeout,
}

/// Terminal report of a supervised child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildExit {
    /// Why it stopped.
    pub reason: ExitReason,
    /// Last [`STDERR_TAIL_BYTES`] of stderr.
    pub stderr_tail: String,
    /// Transcript artifact, when a path was configured and writing succeeded.
    pub transcript: Option<ArtifactRef>,
    /// Spawn → exit.
    pub wall: Duration,
}

impl ChildExit {
    /// `true` for a clean `exit 0`.
    #[must_use]
    pub const fn success(&self) -> bool {
        matches!(self.reason, ExitReason::Exited(0))
    }
}

/// Read counters of a child (for tests and metrics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChildStats {
    /// Bytes read from both pipes.
    pub bytes_read: u64,
    /// Lines delivered (or discarded when the consumer is gone).
    pub lines_read: u64,
    /// Lines that exceeded the cap.
    pub lines_truncated: u64,
}

#[derive(Debug, Default)]
struct Counters {
    bytes_read: AtomicU64,
    lines_read: AtomicU64,
    lines_truncated: AtomicU64,
}

/// Handle on a supervised child process.
#[derive(Debug)]
pub struct ChildHandle {
    pid: u32,
    lines: mpsc::Receiver<OutputLine>,
    exit: oneshot::Receiver<ChildExit>,
    cancel: CancellationToken,
    counters: Arc<Counters>,
}

impl ChildHandle {
    /// Process id (also the process group id).
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Requests termination (SIGTERM → grace → SIGKILL on the whole group).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The token driving cancellation.
    #[must_use]
    pub const fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Next output line; `None` once both pipes reached EOF.
    pub async fn next_line(&mut self) -> Option<OutputLine> {
        self.lines.recv().await
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> ChildStats {
        ChildStats {
            bytes_read: self.counters.bytes_read.load(Ordering::Relaxed),
            lines_read: self.counters.lines_read.load(Ordering::Relaxed),
            lines_truncated: self.counters.lines_truncated.load(Ordering::Relaxed),
        }
    }

    /// Drains remaining lines and returns the exit report.
    pub async fn wait(mut self) -> ChildExit {
        while self.lines.recv().await.is_some() {}
        self.exit.await.unwrap_or_else(|_| ChildExit {
            reason: ExitReason::Signaled(0),
            stderr_tail: "supervisor task vanished".to_owned(),
            transcript: None,
            wall: Duration::ZERO,
        })
    }
}

/// Spawns and supervises worker subprocesses.
#[derive(Debug, Clone, Copy, Default)]
pub struct Supervisor;

impl Supervisor {
    /// A `Command` for `program` with the supervisor's fixed settings applied
    /// (adapters add arguments, then call [`Supervisor::spawn`]).
    #[must_use]
    pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
        Command::new(program)
    }

    /// Spawns `cmd` under supervision. Errors only when the process cannot be
    /// started (missing binary, unusable cwd, transcript directory).
    pub fn spawn(mut cmd: Command, opts: SpawnOpts) -> Result<ChildHandle, WorkerError> {
        let SpawnOpts {
            kind,
            cwd,
            env,
            stdin,
            kill_grace,
            timeout,
            cancel,
            transcript,
            max_line_bytes,
            line_capacity,
        } = opts;

        if !cwd.is_dir() {
            return Err(WorkerError::WorkspaceUnavailable {
                path: cwd,
                reason: "not a directory".to_owned(),
            });
        }
        if let Some(path) = &transcript
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| WorkerError::io(format!("creating {}", parent.display()), e))?;
        }

        let program = cmd.as_std().get_program().to_string_lossy().into_owned();
        cmd.current_dir(&cwd)
            .env_clear()
            .envs(&env)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let start = Instant::now();
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                WorkerError::BinaryMissing {
                    kind,
                    bin: program.clone(),
                }
            } else {
                WorkerError::io(format!("spawning {program}"), e)
            }
        })?;
        let pid = child.id().ok_or_else(|| {
            WorkerError::io(
                format!("spawning {program}"),
                std::io::Error::other("child exited before its pid could be read"),
            )
        })?;
        metrics::histogram!(metric_names::WORKER_SPAWN_DURATION_SECONDS, "worker" => kind.as_str())
            .record(start.elapsed().as_secs_f64());
        metrics::gauge!(metric_names::WORKER_PROCESSES, "worker" => kind.as_str()).increment(1.0);
        tracing::debug!(kind = %kind, pid, program = %program, cwd = %cwd.display(), "spawned worker process");

        let counters = Arc::new(Counters::default());
        let (line_tx, line_rx) = mpsc::channel(line_capacity);
        let (transcript_tx, transcript_task) = match transcript {
            Some(path) => {
                let (tx, rx) = mpsc::channel(TRANSCRIPT_CAPACITY);
                (Some(tx), Some(tokio::spawn(write_transcript(path, rx))))
            }
            None => (None, None),
        };

        if let (Some(payload), Some(mut stdin_pipe)) = (stdin, child.stdin.take()) {
            tokio::spawn(async move {
                // A child that exits without reading stdin yields EPIPE; irrelevant.
                let _ = stdin_pipe.write_all(&payload).await;
                let _ = stdin_pipe.shutdown().await;
            });
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(read_pipe(
            stdout,
            Stream::Stdout,
            max_line_bytes,
            line_tx.clone(),
            transcript_tx.clone(),
            Arc::clone(&counters),
        ));
        let stderr_task = tokio::spawn(read_pipe(
            stderr,
            Stream::Stderr,
            max_line_bytes,
            line_tx,
            transcript_tx,
            Arc::clone(&counters),
        ));

        let (exit_tx, exit_rx) = oneshot::channel();
        let supervisor_cancel = cancel.clone();
        tokio::spawn(supervise(
            child,
            pid,
            kind,
            kill_grace,
            timeout,
            supervisor_cancel,
            start,
            stdout_task,
            stderr_task,
            transcript_task,
            exit_tx,
        ));

        Ok(ChildHandle {
            pid,
            lines: line_rx,
            exit: exit_rx,
            cancel,
            counters,
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    mut child: Child,
    pid: u32,
    kind: WorkerKind,
    kill_grace: Duration,
    timeout: Option<Duration>,
    cancel: CancellationToken,
    start: Instant,
    stdout_task: JoinHandle<String>,
    stderr_task: JoinHandle<String>,
    transcript_task: Option<JoinHandle<std::io::Result<ArtifactRef>>>,
    exit_tx: oneshot::Sender<ChildExit>,
) {
    let deadline = timeout.unwrap_or(Duration::MAX);
    let reason = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            tracing::debug!(kind = %kind, pid, "cancelling worker process group");
            kill_group(&mut child, pid, kill_grace).await;
            ExitReason::Cancelled
        }
        () = tokio::time::sleep(deadline), if timeout.is_some() => {
            tracing::debug!(kind = %kind, pid, "worker timed out; killing process group");
            kill_group(&mut child, pid, kill_grace).await;
            ExitReason::Timeout
        }
        status = child.wait() => match status {
            Ok(status) => status_to_reason(status),
            Err(err) => {
                tracing::warn!(kind = %kind, pid, error = %err, "wait() on worker failed");
                ExitReason::Signaled(0)
            }
        },
    };
    // The leader is gone; make sure no orphaned grandchild keeps the group
    // (and our pipes) alive.
    signal_group(pid, GroupSignal::Kill);

    let _ = stdout_task.await;
    let stderr_tail = stderr_task.await.unwrap_or_default();
    let transcript = match transcript_task {
        Some(task) => match task.await {
            Ok(Ok(artifact)) => Some(artifact),
            Ok(Err(err)) => {
                tracing::warn!(kind = %kind, pid, error = %err, "transcript write failed");
                None
            }
            Err(_) => None,
        },
        None => None,
    };
    let wall = start.elapsed();
    metrics::gauge!(metric_names::WORKER_PROCESSES, "worker" => kind.as_str()).decrement(1.0);
    metrics::counter!(
        metric_names::WORKER_EXITS_TOTAL,
        "worker" => kind.as_str(),
        "class" => exit_class(reason),
    )
    .increment(1);
    let _ = exit_tx.send(ChildExit {
        reason,
        stderr_tail,
        transcript,
        wall,
    });
}

/// The bounded `class` label of `kevin_worker_exits_total` (plan/10 §Metrics).
const fn exit_class(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::Exited(0) => "ok",
        ExitReason::Exited(_) => "permanent",
        ExitReason::Timeout => "timeout",
        ExitReason::Cancelled => "killed",
        ExitReason::Signaled(_) => "transient",
    }
}

fn status_to_reason(status: std::process::ExitStatus) -> ExitReason {
    if let Some(code) = status.code() {
        return ExitReason::Exited(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(sig) = status.signal() {
            return ExitReason::Signaled(sig);
        }
    }
    ExitReason::Signaled(0)
}

#[derive(Debug, Clone, Copy)]
enum GroupSignal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: u32, signal: GroupSignal) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    let sig = match signal {
        GroupSignal::Term => Signal::SIGTERM,
        GroupSignal::Kill => Signal::SIGKILL,
    };
    // ESRCH (group already gone) is the expected failure here.
    let _ = killpg(Pid::from_raw(raw), sig);
}

#[cfg(not(unix))]
fn signal_group(_pid: u32, _signal: GroupSignal) {}

/// SIGTERM the group, wait `grace`, SIGKILL, reap.
async fn kill_group(child: &mut Child, pid: u32, grace: Duration) {
    signal_group(pid, GroupSignal::Term);
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        tracing::debug!(pid, "worker ignored SIGTERM for {grace:?}; sending SIGKILL");
        signal_group(pid, GroupSignal::Kill);
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

#[derive(Debug, Serialize)]
struct TranscriptRecord<'a> {
    ts: String,
    stream: &'static str,
    line: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
}

/// Reads one pipe line by line; returns the stderr tail (empty for stdout).
async fn read_pipe<R>(
    pipe: Option<R>,
    stream: Stream,
    max_line_bytes: usize,
    lines: mpsc::Sender<OutputLine>,
    transcript: Option<mpsc::Sender<String>>,
    counters: Arc<Counters>,
) -> String
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(pipe) = pipe else {
        return String::new();
    };
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, pipe);
    let mut buf = Vec::with_capacity(4096);
    let mut tail = String::new();
    let mut consumer_alive = true;
    loop {
        let Some(truncated) = read_bounded_line(&mut reader, &mut buf, max_line_bytes, &counters)
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        let text = String::from_utf8_lossy(&buf).into_owned();
        counters.lines_read.fetch_add(1, Ordering::Relaxed);
        if truncated {
            counters.lines_truncated.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("kevin_worker_lines_truncated_total").increment(1);
        }
        if stream == Stream::Stderr {
            push_tail(&mut tail, &text);
        }
        if let Some(tx) = &transcript {
            let record = TranscriptRecord {
                ts: Utc::now().to_rfc3339(),
                stream: stream.as_str(),
                line: &text,
                truncated,
            };
            if let Ok(json) = serde_json::to_string(&record) {
                // Writer gone (disk error) → keep reading, just no transcript.
                let _ = tx.send(json).await;
            }
        }
        if consumer_alive
            && lines
                .send(OutputLine {
                    stream,
                    text,
                    truncated,
                })
                .await
                .is_err()
        {
            // Consumer dropped the handle: keep draining so the child never
            // blocks on a full pipe, but stop delivering.
            consumer_alive = false;
        }
    }
    tail
}

fn push_tail(tail: &mut String, line: &str) {
    if !tail.is_empty() {
        tail.push('\n');
    }
    tail.push_str(line);
    if tail.len() > STDERR_TAIL_BYTES {
        let cut = tail.len() - STDERR_TAIL_BYTES;
        let mut idx = cut;
        while !tail.is_char_boundary(idx) {
            idx += 1;
        }
        tail.drain(..idx);
    }
}

/// Reads one line (without `\n`/`\r\n`) into `buf`, never holding more than
/// `max` bytes; returns `Some(truncated)`, or `None` at EOF with nothing read.
async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
    counters: &Counters,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut truncated = false;
    let mut read_any = false;
    loop {
        let (consumed, done) = {
            let chunk = reader.fill_buf().await?;
            if chunk.is_empty() {
                if !read_any {
                    return Ok(None);
                }
                (0, true)
            } else {
                read_any = true;
                if let Some(i) = chunk.iter().position(|b| *b == b'\n') {
                    push_bounded(buf, &chunk[..i], max, &mut truncated);
                    (i + 1, true)
                } else {
                    push_bounded(buf, chunk, max, &mut truncated);
                    (chunk.len(), false)
                }
            }
        };
        reader.consume(consumed);
        counters
            .bytes_read
            .fetch_add(consumed as u64, Ordering::Relaxed);
        if done {
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(Some(truncated));
        }
    }
}

fn push_bounded(buf: &mut Vec<u8>, bytes: &[u8], max: usize, truncated: &mut bool) {
    let room = max.saturating_sub(buf.len());
    if bytes.len() > room {
        buf.extend_from_slice(&bytes[..room]);
        *truncated = true;
    } else {
        buf.extend_from_slice(bytes);
    }
}

async fn write_transcript(
    path: PathBuf,
    mut rx: mpsc::Receiver<String>,
) -> std::io::Result<ArtifactRef> {
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let mut writer = BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut bytes: u64 = 0;
    // `plan/09-security.md` §Redaction: the transcript is a persisted sink, so
    // every line goes through the redaction layer and is capped at
    // `TRANSCRIPT_LINE_CAP_BYTES` before it reaches the disk. A worker that
    // runs `cat .env` must not leave the credential in `data_dir`.
    let redactor = kevin_telemetry::redact::Redactor::global();
    while let Some(line) = rx.recv().await {
        let mut line = kevin_telemetry::redact::truncate(
            redactor.redact_str(&line).as_ref(),
            kevin_telemetry::redact::TRANSCRIPT_LINE_CAP_BYTES,
        );
        line.push('\n');
        writer.write_all(line.as_bytes()).await?;
        hasher.update(line.as_bytes());
        bytes += line.len() as u64;
    }
    writer.flush().await?;
    Ok(transcript_ref(&path, &hasher.finalize(), bytes))
}

fn transcript_ref(path: &Path, digest: &[u8], bytes: u64) -> ArtifactRef {
    ArtifactRef {
        id: uuid::Uuid::now_v7(),
        kind: ArtifactKind::Transcript,
        uri: format!("file://{}", path.display()),
        sha256: hex(digest),
        bytes,
    }
}

/// Hex sha256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Path of an attempt transcript:
/// `<data_dir>/runs/<run_id>/<task_id>/<attempt_id>.jsonl`.
#[must_use]
pub fn transcript_path(
    data_dir: &Path,
    run_id: &kevin_domain::RunId,
    task_id: &kevin_domain::TaskId,
    attempt_id: &kevin_domain::AttemptId,
) -> PathBuf {
    data_dir
        .join("runs")
        .join(run_id.to_string())
        .join(task_id.to_string())
        .join(format!("{attempt_id}.jsonl"))
}

/// Outcome of [`classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Exit 0 with a `Final` seen.
    Succeeded,
    /// Anything else.
    Failed {
        /// Classification.
        class: FailureClass,
        /// Diagnostic (includes the stderr tail).
        message: String,
    },
}

/// Exit-classification table (`plan/04-workers.md` §Subprocess supervisor):
///
/// | reason | `saw_final` | verdict |
/// |---|---|---|
/// | killed by us on cancel | — | `Failed{Cancelled}` |
/// | killed by us on timeout | — | `Failed{Transient, "timeout"}` |
/// | exit 0 | yes | `Succeeded` |
/// | exit 0 | no | `Failed{Permanent, "no final message"}` |
/// | exit 137 | — | `Failed{Transient}` (OOM/kill) |
/// | non-zero, stderr matches rate-limit/network patterns | — | `Failed{Transient}` |
/// | other non-zero | — | `Failed{Permanent}` |
/// | killed by a signal (not us) | — | `Failed{Transient}` (worker crashed) |
#[must_use]
pub fn classify(exit: &ChildExit, saw_final: bool) -> Verdict {
    let tail = exit.stderr_tail.trim();
    let with_tail = |prefix: String| {
        if tail.is_empty() {
            prefix
        } else {
            format!("{prefix}: {tail}")
        }
    };
    match exit.reason {
        ExitReason::Cancelled => Verdict::Failed {
            class: FailureClass::Cancelled,
            message: "cancelled".to_owned(),
        },
        ExitReason::Timeout => Verdict::Failed {
            class: FailureClass::Transient,
            message: "timeout".to_owned(),
        },
        ExitReason::Exited(0) if saw_final => Verdict::Succeeded,
        ExitReason::Exited(0) => Verdict::Failed {
            class: FailureClass::Permanent,
            message: "no final message".to_owned(),
        },
        ExitReason::Exited(code) => {
            let class = if code == 137 || is_transient_signature(tail) {
                FailureClass::Transient
            } else {
                FailureClass::Permanent
            };
            Verdict::Failed {
                class,
                message: with_tail(format!("exit {code}")),
            }
        }
        ExitReason::Signaled(sig) => Verdict::Failed {
            class: FailureClass::Transient,
            message: with_tail(format!("killed by signal {sig}")),
        },
    }
}

/// Whether `stderr` looks like a rate-limit / network / overload failure.
#[must_use]
pub fn is_transient_signature(stderr: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b429\b|rate.?limit|too many requests|overloaded|ECONNRESET|ECONNREFUSED|ETIMEDOUT|EAI_AGAIN|ENOTFOUND|EPIPE|socket hang up|\b50[234]\b|service unavailable|network error|connection reset|temporarily unavailable",
        )
        .unwrap_or_else(|e| unreachable!("static regex is valid: {e}"))
    });
    re.is_match(stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(reason: ExitReason, stderr: &str) -> ChildExit {
        ChildExit {
            reason,
            stderr_tail: stderr.to_owned(),
            transcript: None,
            wall: Duration::ZERO,
        }
    }

    #[test]
    fn classification_table() {
        let cases: Vec<(ChildExit, bool, Option<FailureClass>)> = vec![
            (
                exit(ExitReason::Cancelled, ""),
                true,
                Some(FailureClass::Cancelled),
            ),
            (
                exit(ExitReason::Timeout, ""),
                true,
                Some(FailureClass::Transient),
            ),
            (exit(ExitReason::Exited(0), ""), true, None),
            (
                exit(ExitReason::Exited(0), ""),
                false,
                Some(FailureClass::Permanent),
            ),
            (
                exit(ExitReason::Exited(1), "boom"),
                true,
                Some(FailureClass::Permanent),
            ),
            (
                exit(ExitReason::Exited(1), "HTTP 429 Too Many Requests"),
                true,
                Some(FailureClass::Transient),
            ),
            (
                exit(ExitReason::Exited(2), "read ECONNRESET"),
                false,
                Some(FailureClass::Transient),
            ),
            (
                exit(ExitReason::Exited(1), "api overloaded"),
                false,
                Some(FailureClass::Transient),
            ),
            (
                exit(ExitReason::Exited(137), ""),
                false,
                Some(FailureClass::Transient),
            ),
            (
                exit(ExitReason::Signaled(9), ""),
                false,
                Some(FailureClass::Transient),
            ),
            (
                exit(ExitReason::Signaled(6), "abort"),
                true,
                Some(FailureClass::Transient),
            ),
        ];
        for (e, saw_final, expected) in cases {
            let verdict = classify(&e, saw_final);
            match (expected, &verdict) {
                (None, Verdict::Succeeded) => {}
                (Some(class), Verdict::Failed { class: got, .. }) if *got == class => {}
                _ => panic!("{e:?} saw_final={saw_final} → {verdict:?}"),
            }
        }
        match classify(&exit(ExitReason::Exited(3), "bad thing"), true) {
            Verdict::Failed { message, .. } => assert_eq!(message, "exit 3: bad thing"),
            Verdict::Succeeded => panic!(),
        }
        match classify(&exit(ExitReason::Timeout, "x"), true) {
            Verdict::Failed { message, .. } => assert_eq!(message, "timeout"),
            Verdict::Succeeded => panic!(),
        }
    }

    #[test]
    fn stderr_tail_is_bounded() {
        let mut tail = String::new();
        for i in 0..1000 {
            push_tail(&mut tail, &format!("line {i} ééé"));
        }
        assert!(tail.len() <= STDERR_TAIL_BYTES);
        assert!(tail.ends_with("line 999 ééé"));
    }

    #[tokio::test]
    async fn bounded_line_reader_truncates_and_strips_crlf() {
        let data: &[u8] = b"short\r\nthis-line-is-long\nlast";
        let mut reader = BufReader::with_capacity(4, data);
        let counters = Counters::default();
        let mut buf = Vec::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 8, &counters)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(buf, b"short");
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 8, &counters)
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(buf, b"this-lin");
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 8, &counters)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(buf, b"last");
        assert_eq!(
            read_bounded_line(&mut reader, &mut buf, 8, &counters)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            counters.bytes_read.load(Ordering::Relaxed),
            data.len() as u64
        );
    }

    #[test]
    fn transcript_path_layout_and_hex() {
        let p = transcript_path(
            Path::new("/data"),
            &kevin_domain::RunId::nil(),
            &kevin_domain::TaskId::nil(),
            &kevin_domain::AttemptId::nil(),
        );
        assert_eq!(
            p,
            PathBuf::from(
                "/data/runs/00000000-0000-0000-0000-000000000000/00000000-0000-0000-0000-000000000000/00000000-0000-0000-0000-000000000000.jsonl"
            )
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
