//! The [`Worker`] trait and its surrounding types (`plan/04-workers.md` §Core types).
//!
//! A worker's `start` only spawns; everything that happens afterwards flows
//! through the bounded [`WorkerHandle::events`] channel and ends in exactly one
//! terminal event ([`WorkerEvent::Final`] or [`WorkerEvent::Failed`]) that is
//! mirrored by the [`WorkerOutcome`] returned from [`WorkerHandle::wait`].
//!
//! Contract every adapter must honour (checked by [`check_contract`] in tests
//! and enforced at runtime by [`EventSink`]):
//! 1. the first event is `Started`;
//! 2. exactly one terminal event;
//! 3. nothing after the terminal event;
//! 4. cancellation terminates within `workers.kill_grace`.

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use kevin_domain::{FailureClass, ModelAlias, WorkerKind};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::types::{ArtifactRef, ConfigError, ModelEntry, TaskAttemptRequest, Usage};

/// Capacity of the [`WorkerHandle::events`] channel (back-pressure on the child).
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Worker-native session id, used for follow-ups/resume.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerSessionId(pub String);

impl WorkerSessionId {
    /// Wraps a session id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkerSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for WorkerSessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for WorkerSessionId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Normalised stream event of a running attempt.
///
/// Serde form is internally tagged on `type` in `snake_case` (what
/// `orch.task_log` stores).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// The worker process / session is up.
    Started {
        /// Worker-native session id, when known at start.
        session_id: Option<WorkerSessionId>,
        /// Process id (none for the in-process fake).
        pid: Option<u32>,
    },
    /// Assistant text delta.
    AssistantText {
        /// Text fragment.
        delta: String,
    },
    /// Thinking delta (only when the CLI exposes it).
    Thinking {
        /// Text fragment.
        delta: String,
    },
    /// A tool was invoked.
    ToolCall {
        /// Tool name.
        name: String,
        /// First ~200 chars of the input.
        input_summary: String,
    },
    /// A tool returned.
    ToolResult {
        /// Tool name.
        name: String,
        /// Whether it succeeded.
        ok: bool,
        /// Short output summary.
        output_summary: String,
    },
    /// Additive usage delta; `cost_usd` only if reported by the CLI.
    Usage {
        /// Delta since the previous `Usage` event.
        delta: Usage,
    },
    /// The worker needs input (folded into `task.input_requested`).
    InputRequested {
        /// Question text.
        question: String,
        /// Options, possibly empty.
        options: Vec<String>,
    },
    /// Terminal: success.
    Final {
        /// Final answer text.
        text: String,
        /// Structured output when requested and produced.
        structured: Option<serde_json::Value>,
        /// Total usage of the attempt.
        usage: Usage,
    },
    /// Terminal: failure.
    Failed {
        /// Why.
        class: FailureClass,
        /// Diagnostic (stderr tail, error message).
        message: String,
        /// Usage accumulated before failing.
        usage: Usage,
    },
}

impl WorkerEvent {
    /// `true` for `Final` and `Failed`.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, WorkerEvent::Final { .. } | WorkerEvent::Failed { .. })
    }

    /// `snake_case` variant name (the `type` tag).
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            WorkerEvent::Started { .. } => "started",
            WorkerEvent::AssistantText { .. } => "assistant_text",
            WorkerEvent::Thinking { .. } => "thinking",
            WorkerEvent::ToolCall { .. } => "tool_call",
            WorkerEvent::ToolResult { .. } => "tool_result",
            WorkerEvent::Usage { .. } => "usage",
            WorkerEvent::InputRequested { .. } => "input_requested",
            WorkerEvent::Final { .. } => "final",
            WorkerEvent::Failed { .. } => "failed",
        }
    }
}

/// Terminal state of an attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkerOutcome {
    /// The attempt produced a final answer.
    Succeeded {
        /// Final answer text.
        text: String,
        /// Structured output, if requested and produced.
        structured: Option<serde_json::Value>,
        /// Total usage.
        usage: Usage,
        /// Worker-native session id for follow-ups.
        session_id: Option<WorkerSessionId>,
        /// Raw transcript artifact.
        transcript: ArtifactRef,
    },
    /// The attempt failed.
    Failed {
        /// Why.
        class: FailureClass,
        /// Diagnostic.
        message: String,
        /// Usage accumulated before failing.
        usage: Usage,
        /// Raw transcript artifact, when one was written.
        transcript: Option<ArtifactRef>,
    },
}

impl WorkerOutcome {
    /// `true` for `Succeeded`.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, WorkerOutcome::Succeeded { .. })
    }

    /// The failure class, if failed.
    #[must_use]
    pub const fn failure_class(&self) -> Option<FailureClass> {
        match self {
            WorkerOutcome::Succeeded { .. } => None,
            WorkerOutcome::Failed { class, .. } => Some(*class),
        }
    }

    /// Usage of the attempt.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        match self {
            WorkerOutcome::Succeeded { usage, .. } | WorkerOutcome::Failed { usage, .. } => usage,
        }
    }

    /// Transcript artifact, if any.
    #[must_use]
    pub const fn transcript(&self) -> Option<&ArtifactRef> {
        match self {
            WorkerOutcome::Succeeded { transcript, .. } => Some(transcript),
            WorkerOutcome::Failed { transcript, .. } => transcript.as_ref(),
        }
    }

    /// A failure outcome without transcript.
    pub fn failed(class: FailureClass, message: impl Into<String>, usage: Usage) -> Self {
        WorkerOutcome::Failed {
            class,
            message: message.into(),
            usage,
            transcript: None,
        }
    }
}

/// Whether a worker CLI can authenticate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "hint", rename_all = "snake_case")]
pub enum AuthStatus {
    /// Credentials found / `auth status` ok.
    Ready,
    /// Not authenticated; the hint says what to do.
    Missing(String),
    /// Could not determine (adapter does not know how, or probe failed).
    Unknown,
}

impl fmt::Display for AuthStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthStatus::Ready => f.write_str("ready"),
            AuthStatus::Missing(hint) => write!(f, "missing ({hint})"),
            AuthStatus::Unknown => f.write_str("unknown"),
        }
    }
}

/// Result of a worker health probe (`kevin workers doctor`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doctor {
    /// Which worker.
    pub kind: WorkerKind,
    /// Resolved binary path, `None` when missing.
    pub binary: Option<PathBuf>,
    /// Version string reported by the binary, if obtainable.
    pub version: Option<String>,
    /// Auth readiness.
    pub auth_ready: AuthStatus,
    /// Free-form remarks (hints, warnings).
    pub notes: Vec<String>,
}

impl Doctor {
    /// A doctor report for a binary that could not be found.
    pub fn missing(kind: WorkerKind, bin: &str) -> Self {
        Self {
            kind,
            binary: None,
            version: None,
            auth_ready: AuthStatus::Unknown,
            notes: vec![format!(
                "missing (workers.{kind}.bin = {bin:?}) → disable or install"
            )],
        }
    }

    /// `true` when the binary exists and auth is not known to be missing.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.binary.is_some() && !matches!(self.auth_ready, AuthStatus::Missing(_))
    }
}

/// Spawn-time errors. Runtime failures arrive as [`WorkerEvent::Failed`], never as `Err`.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The configured binary is not on `PATH` / does not exist.
    #[error("worker `{kind}` binary not found (workers.{kind}.bin = {bin:?})")]
    BinaryMissing {
        /// Worker kind.
        kind: WorkerKind,
        /// Configured binary.
        bin: String,
    },
    /// The model alias cannot be used by this worker.
    #[error("model alias `{alias}` is invalid for this worker: {reason}")]
    InvalidAlias {
        /// Alias.
        alias: ModelAlias,
        /// Why.
        reason: String,
    },
    /// The workspace directory is not usable.
    #[error("workspace {path:?} unavailable: {reason}")]
    WorkspaceUnavailable {
        /// Path.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// A dangerous flag was requested outside the `container` sandbox tier.
    #[error("policy violation: flag `{flag}` is forbidden under sandbox tier `{tier}`")]
    PolicyViolation {
        /// Offending flag/value.
        flag: String,
        /// Effective tier.
        tier: String,
    },
    /// An IO error while preparing the attempt (transcript directory, pipes…).
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl WorkerError {
    /// Helper for [`WorkerError::Io`].
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        WorkerError::Io {
            context: context.into(),
            source,
        }
    }

    /// How the orchestrator should classify an attempt that failed to start.
    ///
    /// A missing binary, an invalid alias or a policy violation will be exactly
    /// as broken on the next attempt: retrying burns the task's attempt budget
    /// and hides the real cause behind "max attempts exhausted". Only genuine
    /// IO — a full disk, an unwritable transcript directory, a workspace that
    /// has not appeared yet — is worth retrying.
    #[must_use]
    pub const fn failure_class(&self) -> FailureClass {
        match self {
            WorkerError::BinaryMissing { .. }
            | WorkerError::InvalidAlias { .. }
            | WorkerError::PolicyViolation { .. } => FailureClass::Permanent,
            WorkerError::WorkspaceUnavailable { .. } | WorkerError::Io { .. } => {
                FailureClass::Transient
            }
        }
    }
}

/// A worker adapter: drives one CLI (or the in-process fake).
#[async_trait]
pub trait Worker: Send + Sync {
    /// Which CLI this adapter drives.
    fn kind(&self) -> WorkerKind;

    /// Health probe: binary, version, auth.
    async fn doctor(&self) -> Doctor;

    /// Validates worker-specific extra keys of a `[models.<alias>]` entry.
    fn validate_alias(&self, alias: &ModelAlias, entry: &ModelEntry) -> Result<(), ConfigError>;

    /// Spawns the attempt. `Err` only when it cannot be spawned.
    async fn start(&self, req: TaskAttemptRequest) -> Result<WorkerHandle, WorkerError>;
}

/// Sending side of a [`WorkerHandle`], given to the adapter's driver task.
///
/// Enforces the stream contract: a synthetic `Started` is emitted if the
/// driver forgets it, events after the terminal one are dropped (and logged),
/// and `Started.session_id` is published on the session watch.
#[derive(Debug)]
pub struct EventSink {
    kind: WorkerKind,
    tx: mpsc::Sender<WorkerEvent>,
    session: watch::Sender<Option<WorkerSessionId>>,
    started: bool,
    terminated: bool,
}

impl EventSink {
    /// Emits an event, awaiting channel capacity (back-pressure). Returns
    /// `false` when the consumer is gone or the stream already terminated.
    pub async fn emit(&mut self, event: WorkerEvent) -> bool {
        if self.terminated {
            tracing::warn!(
                kind = %self.kind,
                event = event.kind_name(),
                "worker emitted an event after the terminal one; dropped"
            );
            return false;
        }
        if !self.started {
            self.started = true;
            if let WorkerEvent::Started { session_id, .. } = &event {
                if session_id.is_some() {
                    self.session.send_replace(session_id.clone());
                }
            } else {
                tracing::debug!(kind = %self.kind, "inserting synthetic Started event");
                if self
                    .tx
                    .send(WorkerEvent::Started {
                        session_id: None,
                        pid: None,
                    })
                    .await
                    .is_err()
                {
                    return false;
                }
            }
        }
        if event.is_terminal() {
            self.terminated = true;
        }
        self.tx.send(event).await.is_ok()
    }

    /// Publishes a session id learned after `Started`.
    pub fn set_session(&self, session_id: WorkerSessionId) {
        self.session.send_replace(Some(session_id));
    }

    /// `true` once a terminal event was emitted.
    #[must_use]
    pub const fn is_terminated(&self) -> bool {
        self.terminated
    }

    /// `true` when the consumer dropped its receiver.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}

/// Handle on a running attempt.
#[derive(Debug)]
pub struct WorkerHandle {
    /// Bounded (cap [`EVENT_CHANNEL_CAPACITY`]) → back-pressure on the child.
    pub events: mpsc::Receiver<WorkerEvent>,
    /// Worker-native session id once known.
    pub session_id: watch::Receiver<Option<WorkerSessionId>>,
    cancel: CancellationToken,
    join: JoinHandle<()>,
    outcome: oneshot::Receiver<WorkerOutcome>,
}

impl WorkerHandle {
    /// Spawns `driver` as the attempt's task. The driver emits events through
    /// the [`EventSink`] and returns the [`WorkerOutcome`]; its terminal event
    /// and outcome must agree.
    pub fn spawn<F, Fut>(kind: WorkerKind, cancel: CancellationToken, driver: F) -> Self
    where
        F: FnOnce(EventSink) -> Fut,
        Fut: Future<Output = WorkerOutcome> + Send + 'static,
    {
        let (tx, events) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (session_tx, session_id) = watch::channel(None);
        let (outcome_tx, outcome) = oneshot::channel();
        let sink = EventSink {
            kind,
            tx,
            session: session_tx,
            started: false,
            terminated: false,
        };
        let fut = driver(sink);
        let join = tokio::spawn(async move {
            let outcome = fut.await;
            record_outcome_metrics(kind, &outcome);
            let _ = outcome_tx.send(outcome);
        });
        Self {
            events,
            session_id,
            cancel,
            join,
            outcome,
        }
    }

    /// Requests cancellation (SIGTERM → grace → SIGKILL for subprocesses).
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The attempt's cancellation token.
    #[must_use]
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Next event, `None` once the stream is closed.
    pub async fn next_event(&mut self) -> Option<WorkerEvent> {
        self.events.recv().await
    }

    /// `true` once the driver task finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    /// Drains remaining events and returns the terminal state.
    pub async fn wait(mut self) -> WorkerOutcome {
        while self.events.recv().await.is_some() {}
        let joined = self.join.await;
        if let Ok(outcome) = self.outcome.await {
            return outcome;
        }
        let reason = match joined {
            Err(err) if err.is_panic() => "worker driver panicked",
            Err(_) => "worker driver was aborted",
            Ok(()) => "worker driver finished without an outcome",
        };
        WorkerOutcome::failed(FailureClass::Transient, reason, Usage::default())
    }

    /// Collects every event until the stream closes, then the outcome.
    pub async fn collect(mut self) -> (Vec<WorkerEvent>, WorkerOutcome) {
        let mut events = Vec::new();
        while let Some(ev) = self.events.recv().await {
            events.push(ev);
        }
        let outcome = self.wait().await;
        (events, outcome)
    }
}

/// `kevin_worker_exit_total{kind,class}` and `kevin_worker_tokens_total{kind,direction}`
/// (`plan/04-workers.md` §Subprocess supervisor, Metrics).
fn record_outcome_metrics(kind: WorkerKind, outcome: &WorkerOutcome) {
    let class = outcome
        .failure_class()
        .map_or("succeeded", FailureClass::as_str);
    metrics::counter!("kevin_worker_exit_total", "kind" => kind.as_str(), "class" => class)
        .increment(1);
    let usage = outcome.usage();
    metrics::counter!("kevin_worker_tokens_total", "kind" => kind.as_str(), "direction" => "input")
        .increment(usage.input_tokens);
    metrics::counter!("kevin_worker_tokens_total", "kind" => kind.as_str(), "direction" => "output")
        .increment(usage.output_tokens);
}

/// A violation of the worker stream contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractViolation {
    /// Stream was empty.
    #[error("no events emitted")]
    Empty,
    /// First event is not `Started`.
    #[error("first event is `{0}`, expected `started`")]
    NotStartedFirst(&'static str),
    /// No terminal event.
    #[error("no terminal event")]
    NoTerminal,
    /// More than one terminal event.
    #[error("{0} terminal events, expected exactly one")]
    MultipleTerminal(usize),
    /// Something after the terminal event.
    #[error("event `{0}` after the terminal event")]
    AfterTerminal(&'static str),
}

/// Checks the stream contract on a collected event sequence.
pub fn check_contract(events: &[WorkerEvent]) -> Result<(), ContractViolation> {
    let Some(first) = events.first() else {
        return Err(ContractViolation::Empty);
    };
    if !matches!(first, WorkerEvent::Started { .. }) {
        return Err(ContractViolation::NotStartedFirst(first.kind_name()));
    }
    let terminals = events.iter().filter(|e| e.is_terminal()).count();
    match terminals {
        0 => return Err(ContractViolation::NoTerminal),
        1 => {}
        n => return Err(ContractViolation::MultipleTerminal(n)),
    }
    if let Some(pos) = events.iter().position(WorkerEvent::is_terminal)
        && let Some(after) = events.get(pos + 1)
    {
        return Err(ContractViolation::AfterTerminal(after.kind_name()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> WorkerEvent {
        WorkerEvent::Started {
            session_id: None,
            pid: None,
        }
    }

    fn final_ev() -> WorkerEvent {
        WorkerEvent::Final {
            text: "ok".into(),
            structured: None,
            usage: Usage::default(),
        }
    }

    #[test]
    fn events_serde_is_tagged_snake_case() {
        let ev = WorkerEvent::ToolCall {
            name: "edit".into(),
            input_summary: "src/auth.rs".into(),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "tool_call");
        assert_eq!(json["name"], "edit");
        let back: WorkerEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
        let failed = WorkerEvent::Failed {
            class: FailureClass::Transient,
            message: "timeout".into(),
            usage: Usage::default(),
        };
        let json = serde_json::to_value(&failed).unwrap();
        assert_eq!(json["type"], "failed");
        assert_eq!(json["class"], "transient");
        assert!(failed.is_terminal());
        assert!(!started().is_terminal());
    }

    #[test]
    fn contract_checker_flags_each_violation() {
        assert_eq!(check_contract(&[]), Err(ContractViolation::Empty));
        assert_eq!(
            check_contract(&[final_ev()]),
            Err(ContractViolation::NotStartedFirst("final"))
        );
        assert_eq!(
            check_contract(&[started()]),
            Err(ContractViolation::NoTerminal)
        );
        assert_eq!(
            check_contract(&[started(), final_ev(), final_ev()]),
            Err(ContractViolation::MultipleTerminal(2))
        );
        assert_eq!(
            check_contract(&[
                started(),
                final_ev(),
                WorkerEvent::AssistantText { delta: "x".into() }
            ]),
            Err(ContractViolation::AfterTerminal("assistant_text"))
        );
        assert_eq!(check_contract(&[started(), final_ev()]), Ok(()));
    }

    #[tokio::test]
    async fn sink_inserts_started_and_drops_after_terminal() {
        let handle = WorkerHandle::spawn(
            WorkerKind::Fake,
            CancellationToken::new(),
            |mut sink| async move {
                assert!(
                    sink.emit(WorkerEvent::AssistantText { delta: "a".into() })
                        .await
                );
                assert!(sink.emit(final_ev()).await);
                assert!(
                    !sink
                        .emit(WorkerEvent::AssistantText { delta: "b".into() })
                        .await
                );
                WorkerOutcome::failed(FailureClass::Permanent, "unused", Usage::default())
            },
        );
        let (events, outcome) = handle.collect().await;
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], WorkerEvent::Started { .. }));
        assert!(check_contract(&events).is_ok());
        assert_eq!(outcome.failure_class(), Some(FailureClass::Permanent));
    }

    #[tokio::test]
    async fn handle_wait_survives_a_panicking_driver() {
        let handle = WorkerHandle::spawn(
            WorkerKind::Fake,
            CancellationToken::new(),
            |_sink| async move {
                panic!("boom");
            },
        );
        let outcome = handle.wait().await;
        assert_eq!(outcome.failure_class(), Some(FailureClass::Transient));
    }

    #[tokio::test]
    async fn session_watch_is_published_from_started() {
        let handle = WorkerHandle::spawn(
            WorkerKind::Fake,
            CancellationToken::new(),
            |mut sink| async move {
                sink.emit(WorkerEvent::Started {
                    session_id: Some("s-1".into()),
                    pid: Some(42),
                })
                .await;
                sink.emit(final_ev()).await;
                WorkerOutcome::failed(FailureClass::Permanent, "unused", Usage::default())
            },
        );
        let mut session = handle.session_id.clone();
        session.changed().await.unwrap();
        assert_eq!(
            session.borrow().as_ref().map(WorkerSessionId::as_str),
            Some("s-1")
        );
        let _ = handle.wait().await;
    }

    #[test]
    fn doctor_missing_is_unhealthy() {
        let d = Doctor::missing(WorkerKind::Codex, "codex");
        assert!(!d.is_healthy());
        assert!(d.notes[0].contains("workers.codex.bin"));
        assert_eq!(
            AuthStatus::Missing("run x".into()).to_string(),
            "missing (run x)"
        );
    }
}
