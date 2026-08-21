//! Adapter for the `opencode` CLI (`plan/04-workers.md` §Adapter: opencode).
//!
//! The adapter builds the exact command line of the plan, drives it through
//! the shared [`crate::supervisor`], and normalises the `opencode run --format
//! json` stream into [`WorkerEvent`]s:
//!
//! | `--format json` line | [`WorkerEvent`] |
//! |---|---|
//! | any line (first one only) | `Started{session_id = line.sessionID}` |
//! | `{"type":"text","part":{…}}` | `AssistantText` |
//! | `{"type":"reasoning","part":{…}}` | `Thinking` |
//! | `{"type":"tool_use","part":{…}}` | `ToolCall` **and** `ToolResult{ok}` |
//! | `{"type":"step_finish","part":{tokens,cost}}` | `Usage{delta}` |
//! | `{"type":"error","error":{name,data}}` | `Failed` |
//!
//! `step_start` lines and malformed JSON are transcript-only. Golden fixtures
//! under `tests/fixtures/opencode/` pin the shapes against a real capture of
//! `opencode` 1.18.15 (see `inferred.meta.toml`).
//!
//! Four things differ from the other adapters:
//!
//! - **No terminal line.** `opencode run` has no `result` / `turn.completed`
//!   equivalent: its emitter loop breaks when the session goes idle and the
//!   process exits (0, or 1 once a `session.error` was seen). The adapter
//!   therefore synthesises the single [`WorkerEvent::Final`] after exit, and
//!   [`OpencodeStream::saw_final`] means "a step completed and no error line
//!   arrived".
//! - **No system-prompt flag and no output-schema flag.** The Kevin briefing
//!   and the JSON-schema instruction both ride in the message, which is the
//!   trailing positional argument (see [`message`]).
//! - **One line per finished tool.** `tool_use` is emitted once, already
//!   carrying the tool's terminal state, so it yields a `ToolCall` immediately
//!   followed by its `ToolResult`.
//! - **Cost is reported.** `step-finish.cost` is a per-step USD amount, so
//!   `Usage::cost_usd` is filled in and the router price table is only a
//!   fallback. `tokens.reasoning` is billed as output, exactly as `opencode
//!   stats` aggregates it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use kevin_config::OpencodeWorker as OpencodeConfig;
use kevin_domain::{Effort, FailureClass, ModelAlias, WorkerKind};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::policy::SandboxPolicy;
use crate::registry::{RegistryConfig, VERSION_PROBE_TIMEOUT, locate_binary, probe_binary};
use crate::structured;
use crate::supervisor::{
    ChildExit, ChildHandle, ExitReason, SpawnOpts, Stream, Supervisor, Verdict, classify,
    transcript_path,
};
use crate::types::{ArtifactRef, ConfigError, ModelEntry, TaskAttemptRequest, Usage};
use crate::worker::{
    AuthStatus, Doctor, EventSink, Worker, WorkerError, WorkerEvent, WorkerHandle, WorkerOutcome,
    WorkerSessionId,
};

/// How much of a tool input/output is kept in an event summary.
pub const SUMMARY_CHARS: usize = 200;

/// Environment variables that on their own prove `opencode` can authenticate.
pub const AUTH_ENV_VARS: &[&str] = &[
    "OPENCODE_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "GEMINI_API_KEY",
];

/// What `opencode providers list` is given before being considered stuck.
pub const PROVIDERS_PROBE_TIMEOUT: Duration = VERSION_PROBE_TIMEOUT;

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// The `opencode` adapter.
#[derive(Debug, Clone)]
pub struct OpencodeWorker {
    cfg: OpencodeConfig,
    policy: SandboxPolicy,
    kill_grace: Duration,
    data_dir: PathBuf,
}

impl OpencodeWorker {
    /// An adapter for `[workers.opencode]` under `policy`.
    pub fn new(
        cfg: OpencodeConfig,
        policy: SandboxPolicy,
        kill_grace: Duration,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cfg,
            policy,
            kill_grace,
            data_dir: data_dir.into(),
        }
    }

    /// Builds the adapter from the registry's configuration slice.
    ///
    /// `--auto` in `workers.opencode.extra_args` outside the `container` tier
    /// is rejected here so `kevin workers doctor` and startup fail loudly
    /// instead of at the first attempt (`plan/09-security.md`).
    pub fn from_registry_config(
        cfg: &RegistryConfig,
        policy: &SandboxPolicy,
    ) -> Result<Self, ConfigError> {
        let worker = Self::new(
            cfg.opencode.clone(),
            *policy,
            cfg.kill_grace,
            cfg.data_dir.clone(),
        );
        policy
            .check_argv(&worker.cfg.extra_args)
            .map_err(|err| ConfigError::Invalid {
                key: "workers.opencode.extra_args".to_owned(),
                layer: kevin_config::Source::Default,
                message: err.to_string(),
            })?;
        Ok(worker)
    }

    /// The `[workers.opencode]` slice in use.
    #[must_use]
    pub const fn config(&self) -> &OpencodeConfig {
        &self.cfg
    }

    /// Overrides `workers.opencode.bin` (the registry applies
    /// `RegistryConfig::with_bin`; tests point it at the `fake-cli` shim).
    pub fn set_bin(&mut self, bin: impl Into<String>) {
        self.cfg.bin = bin.into();
    }

    /// The sandbox policy consulted before every spawn.
    #[must_use]
    pub const fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Transcript root (`<data_dir>/runs/…`).
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The complete argv (program excluded) for `req`
    /// (`plan/04-workers.md` §Adapter: opencode).
    ///
    /// `session` continues an existing opencode session (`-s`); it is used by
    /// follow-up attempts (`context.prior_session`) and by the schema repair
    /// turn. `message` is the trailing positional argument — opencode reads
    /// nothing from stdin.
    pub fn build_argv(
        &self,
        req: &TaskAttemptRequest,
        session: Option<&str>,
        message: &str,
    ) -> Result<Vec<String>, WorkerError> {
        let mut argv: Vec<String> = vec![
            "run".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
            "-m".to_owned(),
            req.model.model.clone(),
            "--dir".to_owned(),
            req.workspace.root.to_string_lossy().into_owned(),
        ];
        if let Some(effort) = req.route.effort {
            argv.push("--variant".to_owned());
            argv.push(effort_flag(effort).to_owned());
        }
        if !self.cfg.agent.trim().is_empty() {
            argv.push("--agent".to_owned());
            argv.push(self.cfg.agent.trim().to_owned());
        }
        if let Some(session) = session.or_else(|| {
            req.context
                .prior_session
                .as_ref()
                .map(WorkerSessionId::as_str)
        }) {
            argv.push("-s".to_owned());
            argv.push(session.to_owned());
        }
        argv.extend(self.cfg.extra_args.iter().cloned());
        // The message is checked too: nothing that reaches the child's argv may
        // carry a forbidden flag, whoever wrote it.
        argv.push(message.to_owned());
        self.policy.check_argv(&argv)?;
        Ok(argv)
    }

    fn spawn_opts(&self, req: &TaskAttemptRequest) -> SpawnOpts {
        SpawnOpts::new(WorkerKind::Opencode, req.workspace.root.clone())
            .env(req.process_env())
            .kill_grace(self.kill_grace)
            .timeout(req.budget.timeout)
            .cancel(req.cancel.clone())
            .transcript(transcript_path(
                &self.data_dir,
                &req.run_id,
                &req.task_id,
                &req.attempt_id,
            ))
    }

    fn spawn(
        &self,
        req: &TaskAttemptRequest,
        message: &str,
        session: Option<&str>,
    ) -> Result<ChildHandle, WorkerError> {
        let argv = self.build_argv(req, session, message)?;
        let mut cmd = Supervisor::command(&self.cfg.bin);
        cmd.args(&argv);
        tracing::debug!(
            kind = %WorkerKind::Opencode,
            bin = %self.cfg.bin,
            model = %req.model.model,
            session = session.is_some(),
            "spawning opencode"
        );
        Supervisor::spawn(cmd, self.spawn_opts(req))
    }
}

/// `Effort` → `opencode run --variant` (`plan/04-workers.md`: `XHigh` → high;
/// the flag is a provider-specific reasoning-effort name).
#[must_use]
pub const fn effort_flag(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High | Effort::XHigh => "high",
        Effort::Max => "max",
    }
}

/// The Kevin briefing: task title, acceptance criteria, operator/lesson
/// context and the memory block. `opencode run` has no `--append-system-prompt`,
/// so it travels inside the message (see [`message`]).
#[must_use]
pub fn briefing(req: &TaskAttemptRequest) -> String {
    let mut out = String::new();
    let mut section = |body: &str| {
        if body.trim().is_empty() {
            return;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(body.trim_end());
    };
    if !req.spec.title.is_empty() {
        section(&format!(
            "# Kevin task\n{} (kind: {})",
            req.spec.title, req.kind
        ));
    }
    if !req.spec.acceptance_criteria.is_empty() {
        let list = req
            .spec
            .acceptance_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        section(&format!("# Acceptance criteria\n{list}"));
    }
    section(&req.context.system_prompt_append);
    if let Some(memory) = &req.context.memory {
        section(memory);
    }
    out
}

/// The trailing positional argument of `opencode run`: briefing, instructions
/// and — when `spec.output_schema` is set — the schema instruction, since
/// `opencode run` has no output-schema flag (`plan/04` §Structured output).
#[must_use]
pub fn message(req: &TaskAttemptRequest) -> String {
    let body = if req.spec.instructions.trim().is_empty() {
        req.prompt_text()
    } else {
        req.spec.instructions.clone()
    };
    let briefing = briefing(req);
    let mut out = if briefing.is_empty() {
        body
    } else {
        format!("{briefing}\n\n# Instructions\n{body}")
    };
    if let Some(schema) = &req.spec.output_schema {
        out.push_str("\n\n# Output\nRespond with only a JSON object matching this schema: ");
        out.push_str(&schema.to_string());
    }
    out
}

#[async_trait]
impl Worker for OpencodeWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Opencode
    }

    async fn doctor(&self) -> Doctor {
        let mut doctor = probe_binary(WorkerKind::Opencode, &self.cfg.bin).await;
        if doctor.binary.is_none() {
            return doctor;
        }
        let env = AUTH_ENV_VARS
            .iter()
            .copied()
            .find(|name| std::env::var(name).is_ok_and(|v| !v.trim().is_empty()));
        let credentials = credentials_file();
        // The credential probe is the last resort: it starts the CLI, so it is
        // only run when neither an env key nor a credential file was found.
        let providers = if env.is_some() || credentials.is_some() {
            None
        } else {
            probe_providers(&self.cfg.bin).await
        };
        doctor.auth_ready = auth_status_from(env, credentials.as_deref(), providers);
        if doctor.auth_ready == AuthStatus::Unknown {
            doctor.notes.push(
                "auth: no credential file, no API key in the environment and \
                 `opencode providers list` did not answer — confirm manually"
                    .to_owned(),
            );
        }
        doctor
    }

    fn validate_alias(&self, alias: &ModelAlias, entry: &ModelEntry) -> Result<(), ConfigError> {
        if entry.worker != WorkerKind::Opencode {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                format!("worker: expected `opencode`, found `{}`", entry.worker),
            ));
        }
        if !is_provider_model(&entry.model) {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                format!(
                    "model: opencode ids are `provider/model` (e.g. \
                     `anthropic/claude-sonnet-5`), found `{}`",
                    entry.model.trim()
                ),
            ));
        }
        if let Some(key) = entry.extra.keys().next() {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                format!("unknown key `{key}`: the opencode worker takes no extra model keys"),
            ));
        }
        Ok(())
    }

    async fn start(&self, req: TaskAttemptRequest) -> Result<WorkerHandle, WorkerError> {
        if locate_binary(&self.cfg.bin).is_none() {
            return Err(WorkerError::BinaryMissing {
                kind: WorkerKind::Opencode,
                bin: self.cfg.bin.clone(),
            });
        }
        // Fail fast on policy violations and an unusable workspace: `start`
        // may only return `Err` for things that prevent spawning at all.
        self.build_argv(&req, None, &message(&req))?;
        if !req.workspace.root.is_dir() {
            return Err(WorkerError::WorkspaceUnavailable {
                path: req.workspace.root.clone(),
                reason: "not a directory".to_owned(),
            });
        }
        let worker = self.clone();
        let cancel = req.cancel.clone();
        Ok(WorkerHandle::spawn(
            WorkerKind::Opencode,
            cancel,
            move |sink| async move { worker.drive(req, sink).await },
        ))
    }
}

/// `true` for a well-formed opencode model id: `<provider>/<model>` with both
/// halves non-empty (the model half may itself contain slashes).
#[must_use]
pub fn is_provider_model(model: &str) -> bool {
    let model = model.trim();
    matches!(model.split_once('/'), Some((provider, rest))
        if !provider.trim().is_empty() && !rest.trim().is_empty())
}

/// Auth readiness from the three offline signals, in priority order: an API
/// key in the environment, a credential file, then the number of credentials
/// `opencode providers list` reported (`None` = the probe did not answer).
///
/// Nothing here ever calls a provider API (`plan/04-workers.md` §Registry and
/// doctor).
#[must_use]
pub fn auth_status_from(
    env_var: Option<&str>,
    credentials_file: Option<&Path>,
    providers: Option<usize>,
) -> AuthStatus {
    if env_var.is_some() {
        return AuthStatus::Ready;
    }
    if credentials_file.is_some_and(has_credentials) {
        return AuthStatus::Ready;
    }
    match providers {
        Some(n) if n > 0 => AuthStatus::Ready,
        Some(_) => AuthStatus::Missing(
            "run `opencode providers login`, or set one of the provider API keys".to_owned(),
        ),
        None if credentials_file.is_some() => AuthStatus::Missing(
            "the opencode credential file holds no credentials — run `opencode providers login`"
                .to_owned(),
        ),
        None => AuthStatus::Unknown,
    }
}

/// `true` when `path` is a JSON object with at least one entry.
fn has_credentials(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.as_object().map(|o| !o.is_empty()))
        .unwrap_or(false)
}

/// The first existing opencode credential file:
/// `$XDG_DATA_HOME/opencode/auth.json`, `~/.local/share/opencode/auth.json`,
/// then the pre-1.x `~/.config/opencode/auth.json`.
#[must_use]
pub fn credentials_file() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        candidates.push(PathBuf::from(data).join("opencode/auth.json"));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local/share/opencode/auth.json"));
        candidates.push(home.join(".config/opencode/auth.json"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Number of credentials `<bin> providers list` reports. Offline: the command
/// only reads the local credential store. `None` when it could not be run or
/// its output was not understood.
pub async fn probe_providers(bin: &str) -> Option<usize> {
    let path = locate_binary(bin)?;
    let mut cmd = tokio::process::Command::new(&path);
    cmd.args(["providers", "list"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(PROVIDERS_PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(count_credentials(&String::from_utf8_lossy(&output.stdout)))
}

/// Counts the `N credentials` / `N environment variables` summary lines of
/// `opencode providers list` (ANSI escapes and box drawing ignored).
#[must_use]
pub fn count_credentials(stdout: &str) -> usize {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)(\d+)\s+(credentials?|environment variables?)\b")
            .unwrap_or_else(|e| unreachable!("static regex is valid: {e}"))
    });
    re.captures_iter(stdout)
        .filter_map(|c| c.get(1)?.as_str().parse::<usize>().ok())
        .sum()
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// What one `opencode run` invocation produced.
struct Turn {
    stream: OpencodeStream,
    exit: ChildExit,
}

impl OpencodeWorker {
    /// Runs the attempt: one turn, plus at most one schema repair turn.
    async fn drive(self, req: TaskAttemptRequest, mut sink: EventSink) -> WorkerOutcome {
        let message = message(&req);
        let mut turn = match self.run_turn(&req, &mut sink, &message, None).await {
            Ok(turn) => turn,
            Err(err) => return fail(&mut sink, FailureClass::Transient, err.to_string()).await,
        };
        let mut transcript = turn.exit.transcript.clone();
        let mut structured = None;

        if let Some(schema) = req.spec.output_schema.clone()
            && matches!(verdict(&turn), Verdict::Succeeded)
        {
            let mut err = match turn.stream.resolve_structured(&schema) {
                Ok(value) => {
                    structured = Some(value);
                    None
                }
                Err(err) => Some(err),
            };
            // One repair turn on the same session (`plan/04` §Structured output).
            if let (Some(violation), Some(session)) = (&err, turn.stream.session_id.clone()) {
                tracing::debug!(error = %violation, "opencode answer failed schema validation; repairing");
                let prompt = structured::repair_prompt(violation);
                match self
                    .run_turn(&req, &mut sink, &prompt, Some(session.as_str()))
                    .await
                {
                    Ok(mut repair) => {
                        transcript = repair.exit.transcript.clone().or(transcript);
                        err = match repair.stream.resolve_structured(&schema) {
                            Ok(value) => {
                                structured = Some(value);
                                None
                            }
                            Err(err) => Some(err),
                        };
                        repair.stream.usage += std::mem::take(&mut turn.stream.usage);
                        turn = repair;
                    }
                    Err(spawn_err) => {
                        tracing::warn!(error = %spawn_err, "schema repair turn could not be spawned");
                    }
                }
            }
            if let Some(violation) = err {
                let usage = turn.stream.finish_usage(&turn.exit);
                let message = format!("schema_violation: {violation}");
                sink.emit(WorkerEvent::Failed {
                    class: FailureClass::Permanent,
                    message: message.clone(),
                    usage: usage.clone(),
                })
                .await;
                return WorkerOutcome::Failed {
                    class: FailureClass::Permanent,
                    message,
                    usage,
                    transcript,
                };
            }
        }

        self.finish(turn, structured, transcript, &mut sink).await
    }

    /// Spawns one `opencode run`, streams its stdout into `sink` and returns
    /// the parsed stream plus the child's exit report. Terminal events are
    /// held back: the driver emits exactly one, after exit classification.
    async fn run_turn(
        &self,
        req: &TaskAttemptRequest,
        sink: &mut EventSink,
        message: &str,
        session: Option<&str>,
    ) -> Result<Turn, WorkerError> {
        let mut child = self.spawn(req, message, session)?;
        let mut stream = OpencodeStream::new(Some(child.pid()));
        if session.is_some() {
            // A continued turn never re-emits `Started`.
            stream.started = true;
            stream.session_id = session.map(WorkerSessionId::new);
        }
        while let Some(line) = child.next_line().await {
            if line.stream != Stream::Stdout {
                continue;
            }
            for event in stream.parse_line(&line.text) {
                if event.is_terminal() {
                    continue;
                }
                if let WorkerEvent::Started {
                    session_id: Some(id),
                    ..
                } = &event
                {
                    sink.set_session(id.clone());
                }
                if !sink.emit(event).await {
                    break;
                }
            }
        }
        let exit = child.wait().await;
        Ok(Turn { stream, exit })
    }

    /// Emits the single terminal event and returns the matching outcome.
    async fn finish(
        &self,
        turn: Turn,
        structured: Option<Value>,
        transcript: Option<ArtifactRef>,
        sink: &mut EventSink,
    ) -> WorkerOutcome {
        let verdict = verdict(&turn);
        let Turn { stream, exit } = turn;
        let usage = stream.finish_usage(&exit);
        match verdict {
            // A success without a transcript is reported as a transient
            // failure: the outcome must always reference the raw stream.
            Verdict::Succeeded if transcript.is_none() => {
                fail(
                    sink,
                    FailureClass::Transient,
                    "transcript could not be written".to_owned(),
                )
                .await
            }
            Verdict::Succeeded => {
                let text = stream.final_text();
                sink.emit(WorkerEvent::Final {
                    text: text.clone(),
                    structured: structured.clone(),
                    usage: usage.clone(),
                })
                .await;
                WorkerOutcome::Succeeded {
                    text,
                    structured,
                    usage,
                    session_id: stream.session_id.clone(),
                    transcript: transcript.expect("checked above"),
                }
            }
            Verdict::Failed { class, message } => {
                sink.emit(WorkerEvent::Failed {
                    class,
                    message: message.clone(),
                    usage: usage.clone(),
                })
                .await;
                WorkerOutcome::Failed {
                    class,
                    message,
                    usage,
                    transcript,
                }
            }
        }
    }
}

/// Exit classification for one turn. Unlike the other adapters, an `error`
/// line is authoritative whatever the exit code: `opencode run` exits 1 after
/// one and writes nothing to stderr, so the generic `exit 1` verdict would
/// lose both the class and the message.
fn verdict(turn: &Turn) -> Verdict {
    match (&turn.stream.terminal, &turn.exit.reason) {
        (_, ExitReason::Cancelled | ExitReason::Timeout) => classify(&turn.exit, false),
        (Some(failure), _) => Verdict::Failed {
            class: failure.class,
            message: failure.message.clone(),
        },
        (None, _) => classify(&turn.exit, turn.stream.saw_final()),
    }
}

async fn fail(sink: &mut EventSink, class: FailureClass, message: String) -> WorkerOutcome {
    sink.emit(WorkerEvent::Failed {
        class,
        message: message.clone(),
        usage: Usage::default(),
    })
    .await;
    WorkerOutcome::failed(class, message, Usage::default())
}

// ---------------------------------------------------------------------------
// `--format json` parser
// ---------------------------------------------------------------------------

/// The failure an `error` line reported (opencode has no success terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalFailure {
    class: FailureClass,
    message: String,
}

/// Incremental parser of `opencode run --format json` output.
///
/// One instance per `opencode run` invocation. [`OpencodeStream::parse_line`]
/// never panics: anything it does not understand is counted and ignored (it
/// still reaches the transcript, which the supervisor writes from the raw
/// pipes).
#[derive(Debug, Default)]
pub struct OpencodeStream {
    pid: Option<u32>,
    started: bool,
    session_id: Option<WorkerSessionId>,
    /// Part ids already turned into events (the CLI may re-emit a part).
    seen_parts: HashSet<String>,
    /// `(message id, text)` for every finished text part, in arrival order.
    texts: Vec<(String, String)>,
    usage: Usage,
    /// At least one step finished — opencode's stand-in for a terminal line.
    completed: bool,
    terminal: Option<TerminalFailure>,
    malformed: u64,
}

impl OpencodeStream {
    /// A parser for a child with this pid.
    #[must_use]
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            ..Self::default()
        }
    }

    /// Session id, taken from the `sessionID` of the first line.
    #[must_use]
    pub const fn session_id(&self) -> Option<&WorkerSessionId> {
        self.session_id.as_ref()
    }

    /// Usage accumulated from every `step_finish` line.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// The final answer: the text parts of the last assistant message, joined
    /// by newlines. Empty when the model answered with tool calls only.
    #[must_use]
    pub fn final_text(&self) -> String {
        let Some((last, _)) = self.texts.last() else {
            return String::new();
        };
        self.texts
            .iter()
            .filter(|(id, _)| id == last)
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Every text part of the turn, joined by newlines (the fallback the
    /// structured-output extraction uses when the last message carries none).
    #[must_use]
    pub fn all_text(&self) -> String {
        self.texts
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `true` when the turn completed: at least one step finished and no
    /// `error` line arrived. `opencode run` emits no terminal line, so this is
    /// what [`classify`] is given in place of "saw a `result`".
    #[must_use]
    pub const fn saw_final(&self) -> bool {
        self.completed && self.terminal.is_none()
    }

    /// Lines that were not valid JSON objects.
    #[must_use]
    pub const fn malformed_lines(&self) -> u64 {
        self.malformed
    }

    /// Total usage of the turn, with the wall clock filled from the child.
    fn finish_usage(&self, exit: &ChildExit) -> Usage {
        let mut usage = self.usage.clone();
        if usage.wall_ms == 0 {
            usage.wall_ms = u64::try_from(exit.wall.as_millis()).unwrap_or(u64::MAX);
        }
        usage
    }

    /// Structured output for `schema`: `opencode run` never returns one
    /// natively, so it is always extracted from the answer text
    /// (`plan/04` §Structured output).
    fn resolve_structured(&self, schema: &Value) -> Result<Value, structured::StructuredError> {
        let final_text = self.final_text();
        match structured::extract_and_validate(&final_text, schema) {
            Ok(value) => Ok(value),
            Err(err) => {
                let all = self.all_text();
                if all == final_text {
                    return Err(err);
                }
                structured::extract_and_validate(&all, schema).map_err(|_| err)
            }
        }
    }

    /// Parses one stdout line into zero or more events.
    #[must_use]
    pub fn parse_line(&mut self, line: &str) -> Vec<WorkerEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            self.malformed += 1;
            tracing::debug!(len = trimmed.len(), "unparsable opencode json line");
            return Vec::new();
        };
        if !value.is_object() {
            self.malformed += 1;
            return Vec::new();
        }
        // Every line carries the session id; the first one starts the stream.
        let mut events = Vec::new();
        if !self.started {
            self.started = true;
            self.session_id = str_at(&value, "sessionID").map(WorkerSessionId::new);
            events.push(WorkerEvent::Started {
                session_id: self.session_id.clone(),
                pid: self.pid,
            });
        }
        let part = value.get("part");
        match str_at(&value, "type") {
            Some("text") => events.extend(self.parse_text(part)),
            Some("reasoning") => events.extend(self.parse_reasoning(part)),
            Some("tool_use") => events.extend(self.parse_tool(part)),
            Some("step_finish") => events.extend(self.parse_step_finish(part)),
            Some("error") => events.extend(self.parse_error(&value)),
            // `step_start` and anything a newer CLI adds: transcript-only.
            _ => {}
        }
        events
    }

    /// `true` the first time this part id is seen (parts may be re-emitted).
    fn first_time(&mut self, part: &Value) -> bool {
        match str_at(part, "id") {
            Some(id) => self.seen_parts.insert(id.to_owned()),
            None => true,
        }
    }

    fn parse_text(&mut self, part: Option<&Value>) -> Vec<WorkerEvent> {
        let Some(part) = part else { return Vec::new() };
        if !self.first_time(part) {
            return Vec::new();
        }
        let Some(text) = str_at(part, "text").filter(|t| !t.is_empty()) else {
            return Vec::new();
        };
        self.texts.push((
            str_at(part, "messageID").unwrap_or_default().to_owned(),
            text.to_owned(),
        ));
        self.completed = true;
        vec![WorkerEvent::AssistantText {
            delta: text.to_owned(),
        }]
    }

    fn parse_reasoning(&mut self, part: Option<&Value>) -> Vec<WorkerEvent> {
        let Some(part) = part else { return Vec::new() };
        if !self.first_time(part) {
            return Vec::new();
        }
        str_at(part, "text")
            .filter(|t| !t.is_empty())
            .map(|text| {
                vec![WorkerEvent::Thinking {
                    delta: text.to_owned(),
                }]
            })
            .unwrap_or_default()
    }

    /// A `tool_use` line always carries the tool's *terminal* state, so it
    /// yields the call and its result at once.
    fn parse_tool(&mut self, part: Option<&Value>) -> Vec<WorkerEvent> {
        let Some(part) = part else { return Vec::new() };
        if !self.first_time(part) {
            return Vec::new();
        }
        let name = str_at(part, "tool").unwrap_or("tool").to_owned();
        let state = part.get("state");
        let input = state.and_then(|s| s.get("input")).map_or_else(
            || "{}".to_owned(),
            |i| serde_json::to_string(i).unwrap_or_default(),
        );
        let status = state.and_then(|s| str_at(s, "status")).unwrap_or("");
        let ok = status == "completed";
        let output = state
            .and_then(|s| str_at(s, if ok { "output" } else { "error" }))
            .unwrap_or_default();
        vec![
            WorkerEvent::ToolCall {
                name: name.clone(),
                input_summary: truncate(&input, SUMMARY_CHARS),
            },
            WorkerEvent::ToolResult {
                name,
                ok,
                output_summary: truncate(output, SUMMARY_CHARS),
            },
        ]
    }

    /// `step-finish` carries this step's tokens and cost — the only place
    /// `opencode run --format json` reports usage.
    fn parse_step_finish(&mut self, part: Option<&Value>) -> Vec<WorkerEvent> {
        let Some(part) = part else { return Vec::new() };
        if !self.first_time(part) {
            return Vec::new();
        }
        self.completed = true;
        let delta = step_usage(part);
        if delta.is_empty() {
            return Vec::new();
        }
        self.usage += delta.clone();
        vec![WorkerEvent::Usage { delta }]
    }

    fn parse_error(&mut self, value: &Value) -> Vec<WorkerEvent> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        let error = value.get("error");
        let name = error
            .and_then(|e| str_at(e, "name"))
            .unwrap_or("UnknownError")
            .to_owned();
        let data = error.and_then(|e| e.get("data"));
        let detail = data
            .and_then(|d| str_at(d, "message"))
            .unwrap_or_default()
            .to_owned();
        let retryable = data
            .and_then(|d| d.get("isRetryable"))
            .and_then(Value::as_bool);
        let status = data
            .and_then(|d| d.get("statusCode"))
            .and_then(Value::as_u64);
        let class = error_failure_class(&name, &detail, retryable, status);
        let message = if detail.trim().is_empty() {
            format!("opencode error `{name}`")
        } else {
            format!("opencode error `{name}`: {}", truncate(&detail, 512))
        };
        self.terminal = Some(TerminalFailure {
            class,
            message: message.clone(),
        });
        vec![WorkerEvent::Failed {
            class,
            message,
            usage: self.usage.clone(),
        }]
    }
}

/// `step-finish.tokens` + `step-finish.cost` → [`Usage`].
///
/// `tokens.reasoning` is added to `output_tokens`, exactly as `opencode stats`
/// aggregates it (`tokens.total = input + output + reasoning + cache.read`).
fn step_usage(part: &Value) -> Usage {
    let tokens = part.get("tokens");
    let at = |key: &str| {
        tokens
            .and_then(|t| t.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cache = |key: &str| {
        tokens
            .and_then(|t| t.get("cache"))
            .and_then(|c| c.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: at("input"),
        output_tokens: at("output").saturating_add(at("reasoning")),
        cache_read_tokens: cache("read"),
        cache_write_tokens: cache("write"),
        cost_usd: decimal_at(part, "cost").filter(|c| !c.is_zero()),
        wall_ms: 0,
    }
}

/// `error.name` (+ its data) → [`FailureClass`].
///
/// The error names are the shipped `NamedError` variants of `opencode`
/// 1.18.15: `APIError`, `ProviderAuthError`, `MessageAbortedError`,
/// `StructuredOutputError`, `ContextOverflowError`, `ContentFilterError`,
/// `MessageOutputLengthError`.
#[must_use]
pub fn error_failure_class(
    name: &str,
    message: &str,
    retryable: Option<bool>,
    status: Option<u64>,
) -> FailureClass {
    if name == "MessageAbortedError" {
        return FailureClass::Cancelled;
    }
    match retryable {
        Some(true) => return FailureClass::Transient,
        Some(false) => return FailureClass::Permanent,
        None => {}
    }
    if matches!(status, Some(408 | 409 | 425 | 429 | 500..=599))
        || crate::supervisor::is_transient_signature(message)
    {
        return FailureClass::Transient;
    }
    FailureClass::Permanent
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn decimal_at(value: &Value, key: &str) -> Option<Decimal> {
    match value.get(key)? {
        Value::Number(n) => n
            .as_f64()
            .and_then(Decimal::from_f64_retain)
            .map(|d| d.round_dp(8)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// First `max` characters (never splits a `char`), with an ellipsis marker.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use kevin_domain::{AttemptId, RunId, TaskId, TaskKind};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::types::{
        AttemptBudget, AttemptContext, EnvAllowlist, Route, TaskSpec, Workspace, WorkspacePolicy,
    };

    fn req() -> TaskAttemptRequest {
        TaskAttemptRequest {
            attempt_id: AttemptId::nil(),
            task_id: TaskId::nil(),
            run_id: RunId::nil(),
            kind: TaskKind::Implement,
            spec: TaskSpec::new("Add auth", "Implement the login flow."),
            route: Route {
                worker: WorkerKind::Opencode,
                model: ModelAlias::new("sonnet5-opencode").unwrap(),
                effort: None,
            },
            model: ModelEntry::new(WorkerKind::Opencode, "anthropic/claude-sonnet-5"),
            workspace: Workspace::in_place("/workspace"),
            context: AttemptContext::default(),
            env: EnvAllowlist::new(["PATH"]),
            budget: AttemptBudget::default(),
            cancel: CancellationToken::new(),
        }
    }

    fn worker() -> OpencodeWorker {
        OpencodeWorker::new(
            OpencodeConfig::default(),
            SandboxPolicy::cli_native(),
            Duration::from_secs(10),
            "/data",
        )
    }

    #[test]
    fn argv_matches_the_plan() {
        let argv = worker().build_argv(&req(), None, "do it").unwrap();
        assert_eq!(
            argv.join(" "),
            "run --format json -m anthropic/claude-sonnet-5 --dir /workspace do it"
        );
    }

    #[test]
    fn argv_adds_variant_agent_session_and_extra_args() {
        let cfg = OpencodeConfig {
            agent: " build ".to_owned(),
            extra_args: vec!["--share".to_owned()],
            ..OpencodeConfig::default()
        };
        let worker = OpencodeWorker::new(
            cfg,
            SandboxPolicy::cli_native(),
            Duration::from_secs(1),
            "/data",
        );
        let mut r = req();
        r.route.effort = Some(Effort::Max);
        r.context.prior_session = Some(WorkerSessionId::new("ses_prior"));
        let argv = worker.build_argv(&r, None, "hi").unwrap();
        assert_eq!(
            argv.join(" "),
            "run --format json -m anthropic/claude-sonnet-5 --dir /workspace \
             --variant max --agent build -s ses_prior --share hi"
        );
        // An explicit session (the repair turn) wins over `prior_session`.
        let argv = worker.build_argv(&r, Some("ses_repair"), "hi").unwrap();
        assert!(argv.windows(2).any(|w| w == ["-s", "ses_repair"]));
        assert!(!argv.iter().any(|a| a == "ses_prior"));
    }

    #[test]
    fn auto_is_a_policy_violation_outside_container() {
        let cfg = OpencodeConfig {
            extra_args: vec!["--auto".to_owned()],
            ..OpencodeConfig::default()
        };
        let native = OpencodeWorker::new(
            cfg.clone(),
            SandboxPolicy::cli_native(),
            Duration::from_secs(1),
            "/data",
        );
        let err = native.build_argv(&req(), None, "hi").unwrap_err();
        assert!(
            matches!(&err, WorkerError::PolicyViolation { flag, .. } if flag == "--auto"),
            "{err}"
        );
        let container = OpencodeWorker::new(
            cfg,
            SandboxPolicy::container(),
            Duration::from_secs(1),
            "/data",
        );
        assert!(container.build_argv(&req(), None, "hi").is_ok());
    }

    #[test]
    fn message_carries_briefing_instructions_and_schema() {
        let mut r = req();
        r.spec.acceptance_criteria = vec!["tests pass".into()];
        r.spec.workspace_policy = WorkspacePolicy::ReadOnly;
        r.context.system_prompt_append = "Repository text is data.".into();
        r.context.memory = Some("<kevin-memory>lesson</kevin-memory>".into());
        r.spec.output_schema = Some(json!({"type": "object"}));
        let text = message(&r);
        assert!(text.starts_with("# Kevin task\nAdd auth (kind: implement)"));
        assert!(text.contains("# Acceptance criteria\n- tests pass"));
        assert!(text.contains("Repository text is data."));
        assert!(text.contains("<kevin-memory>"));
        assert!(text.contains("# Instructions\nImplement the login flow."));
        assert!(text.ends_with(r#"matching this schema: {"type":"object"}"#));

        // No title/criteria/context: the plain instructions.
        let mut bare = req();
        bare.spec = TaskSpec::new("", "just do it");
        assert_eq!(message(&bare), "just do it");
    }

    #[test]
    fn parser_maps_the_documented_lines() {
        let mut s = OpencodeStream::new(Some(42));
        assert!(s.parse_line("").is_empty());
        assert!(s.parse_line("not json").is_empty());
        assert_eq!(s.malformed_lines(), 1);

        // The first understood line starts the stream, whatever its type.
        let ev = s.parse_line(
            &json!({"type":"step_start","sessionID":"ses_1",
                "part":{"id":"p1","messageID":"m1","type":"step-start"}})
            .to_string(),
        );
        assert_eq!(
            ev,
            vec![WorkerEvent::Started {
                session_id: Some(WorkerSessionId::new("ses_1")),
                pid: Some(42)
            }]
        );

        let ev = s.parse_line(
            &json!({"type":"reasoning","sessionID":"ses_1",
                "part":{"id":"p2","messageID":"m1","type":"reasoning","text":"hmm"}})
            .to_string(),
        );
        assert_eq!(
            ev,
            vec![WorkerEvent::Thinking {
                delta: "hmm".into()
            }]
        );

        let ev = s.parse_line(
            &json!({"type":"tool_use","sessionID":"ses_1","part":{"id":"p3","messageID":"m1",
                "type":"tool","tool":"read","callID":"c1",
                "state":{"status":"completed","input":{"filePath":"a.rs"},"output":"fn main() {}"}}})
            .to_string(),
        );
        assert_eq!(
            ev,
            vec![
                WorkerEvent::ToolCall {
                    name: "read".into(),
                    input_summary: r#"{"filePath":"a.rs"}"#.into()
                },
                WorkerEvent::ToolResult {
                    name: "read".into(),
                    ok: true,
                    output_summary: "fn main() {}".into()
                }
            ]
        );
        // A re-emitted part is ignored.
        assert!(
            s.parse_line(
                &json!({"type":"tool_use","sessionID":"ses_1","part":{"id":"p3","type":"tool"}})
                    .to_string()
            )
            .is_empty()
        );

        let ev = s.parse_line(
            &json!({"type":"tool_use","sessionID":"ses_1","part":{"id":"p4","messageID":"m1",
                "type":"tool","tool":"bash","callID":"c2",
                "state":{"status":"error","input":{"command":"false"},"error":"exit 1"}}})
            .to_string(),
        );
        assert_eq!(
            ev[1],
            WorkerEvent::ToolResult {
                name: "bash".into(),
                ok: false,
                output_summary: "exit 1".into()
            }
        );

        let ev = s.parse_line(
            &json!({"type":"text","sessionID":"ses_1",
                "part":{"id":"p5","messageID":"m2","type":"text","text":"done",
                        "time":{"start":1,"end":2}}})
            .to_string(),
        );
        assert_eq!(
            ev,
            vec![WorkerEvent::AssistantText {
                delta: "done".into()
            }]
        );

        let ev = s.parse_line(
            &json!({"type":"step_finish","sessionID":"ses_1","part":{"id":"p6","messageID":"m2",
                "type":"step-finish","reason":"stop","cost":0.0025,
                "tokens":{"total":1330,"input":1200,"output":90,"reasoning":40,
                          "cache":{"read":7,"write":16}}}})
            .to_string(),
        );
        let WorkerEvent::Usage { delta } = &ev[0] else {
            panic!("expected Usage, got {ev:?}");
        };
        assert_eq!(delta.input_tokens, 1200);
        assert_eq!(delta.output_tokens, 130);
        assert_eq!(delta.cache_read_tokens, 7);
        assert_eq!(delta.cache_write_tokens, 16);
        assert_eq!(delta.cost_usd, Some(Decimal::new(25, 4)));

        assert!(s.saw_final());
        assert_eq!(s.final_text(), "done");
        assert_eq!(s.session_id().unwrap().as_str(), "ses_1");
    }

    #[test]
    fn final_text_is_the_last_message_all_text_is_everything() {
        let mut s = OpencodeStream::new(None);
        for (id, msg, text) in [
            ("p1", "m1", "thinking out loud"),
            ("p2", "m2", "first half"),
            ("p3", "m2", "second half"),
        ] {
            let _ = s.parse_line(
                &json!({"type":"text","sessionID":"ses_1",
                    "part":{"id":id,"messageID":msg,"type":"text","text":text}})
                .to_string(),
            );
        }
        assert_eq!(s.final_text(), "first half\nsecond half");
        assert_eq!(s.all_text(), "thinking out loud\nfirst half\nsecond half");
        assert_eq!(OpencodeStream::new(None).final_text(), "");
    }

    #[test]
    fn structured_output_falls_back_to_an_earlier_message() {
        let mut s = OpencodeStream::new(None);
        for (id, msg, text) in [
            ("p1", "m1", r#"{"status": "ok"}"#),
            ("p2", "m2", "All done, see above."),
        ] {
            let _ = s.parse_line(
                &json!({"type":"text","sessionID":"ses_1",
                    "part":{"id":id,"messageID":msg,"type":"text","text":text}})
                .to_string(),
            );
        }
        let schema = json!({"type": "object", "required": ["status"]});
        assert_eq!(s.resolve_structured(&schema).unwrap()["status"], "ok");
        // Nothing anywhere → the error of the final text, not of the fallback.
        let mut empty = OpencodeStream::new(None);
        let _ = empty.parse_line(
            &json!({"type":"text","sessionID":"ses_1",
                "part":{"id":"p1","messageID":"m1","type":"text","text":"no json"}})
            .to_string(),
        );
        assert!(empty.resolve_structured(&schema).is_err());
    }

    #[test]
    fn error_lines_are_classified_and_only_the_first_counts() {
        assert_eq!(
            error_failure_class("MessageAbortedError", "aborted", None, None),
            FailureClass::Cancelled
        );
        assert_eq!(
            error_failure_class("APIError", "rate limited", Some(true), Some(429)),
            FailureClass::Transient
        );
        assert_eq!(
            error_failure_class("APIError", "API key is invalid.", Some(false), Some(401)),
            FailureClass::Permanent
        );
        assert_eq!(
            error_failure_class("APIError", "boom", None, Some(503)),
            FailureClass::Transient
        );
        assert_eq!(
            error_failure_class("ProviderAuthError", "no oauth token", None, None),
            FailureClass::Permanent
        );
        assert_eq!(
            error_failure_class("UnknownError", "socket hang up", None, None),
            FailureClass::Transient
        );

        let mut s = OpencodeStream::new(None);
        let ev = s.parse_line(
            &json!({"type":"error","sessionID":"ses_1",
                "error":{"name":"ContextOverflowError","data":{"message":"too long"}}})
            .to_string(),
        );
        assert!(matches!(
            &ev[1],
            WorkerEvent::Failed { class: FailureClass::Permanent, message, .. }
                if message == "opencode error `ContextOverflowError`: too long"
        ));
        assert!(!s.saw_final());
        // Only the first error line becomes a terminal event.
        assert!(
            s.parse_line(r#"{"type":"error","error":{"name":"APIError"}}"#)
                .is_empty()
        );
    }

    #[test]
    fn helpers_are_char_safe_and_model_ids_validated() {
        assert_eq!(truncate("héllo", 3), "hél…");
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(effort_flag(Effort::XHigh), "high");
        assert!(is_provider_model("anthropic/claude-sonnet-5"));
        assert!(is_provider_model("openrouter/anthropic/claude-3"));
        assert!(!is_provider_model("claude-sonnet-5"));
        assert!(!is_provider_model("/model"));
        assert!(!is_provider_model("provider/"));
        assert_eq!(
            count_credentials("│\n└  4 credentials\n\n└  3 environment variables\n"),
            7
        );
        assert_eq!(count_credentials("nothing here"), 0);
        assert_eq!(count_credentials("└  0 credentials"), 0);
    }

    #[test]
    fn auth_status_prefers_env_then_file_then_providers() {
        assert_eq!(
            auth_status_from(Some("ANTHROPIC_API_KEY"), None, Some(0)),
            AuthStatus::Ready
        );
        assert_eq!(auth_status_from(None, None, Some(2)), AuthStatus::Ready);
        assert!(matches!(
            auth_status_from(None, None, Some(0)),
            AuthStatus::Missing(_)
        ));
        assert_eq!(auth_status_from(None, None, None), AuthStatus::Unknown);
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("auth.json");
        std::fs::write(&full, r#"{"anthropic":{"type":"api","key":"x"}}"#).unwrap();
        assert_eq!(auth_status_from(None, Some(&full), None), AuthStatus::Ready);
        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert!(matches!(
            auth_status_from(None, Some(&empty), None),
            AuthStatus::Missing(_)
        ));
        assert!(!has_credentials(&dir.path().join("nope.json")));
    }

    #[tokio::test]
    async fn validate_alias_rejects_foreign_workers_and_extras() {
        let w = worker();
        let alias = ModelAlias::new("sonnet5-opencode").unwrap();
        assert!(
            w.validate_alias(
                &alias,
                &ModelEntry::new(WorkerKind::Opencode, "anthropic/claude-sonnet-5")
            )
            .is_ok()
        );
        assert!(
            w.validate_alias(&alias, &ModelEntry::new(WorkerKind::Claude, "a/b"))
                .is_err()
        );
        assert!(
            w.validate_alias(
                &alias,
                &ModelEntry::new(WorkerKind::Opencode, "claude-sonnet-5")
            )
            .is_err()
        );
        let mut entry = ModelEntry::new(WorkerKind::Opencode, "anthropic/claude-sonnet-5");
        entry
            .extra
            .insert("provider".to_owned(), toml::Value::String("x".into()));
        assert!(w.validate_alias(&alias, &entry).is_err());
    }

    #[tokio::test]
    async fn start_reports_a_missing_binary() {
        let cfg = OpencodeConfig {
            bin: "definitely-not-opencode-kevin".to_owned(),
            ..OpencodeConfig::default()
        };
        let worker = OpencodeWorker::new(
            cfg,
            SandboxPolicy::cli_native(),
            Duration::from_secs(1),
            "/data",
        );
        assert!(matches!(
            worker.start(req()).await,
            Err(WorkerError::BinaryMissing { .. })
        ));
        let doctor = worker.doctor().await;
        assert!(doctor.binary.is_none());
        assert_eq!(doctor.kind, WorkerKind::Opencode);
        assert!(
            probe_providers("definitely-not-opencode-kevin")
                .await
                .is_none()
        );
    }
}
