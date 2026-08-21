//! Adapter for the `codex` CLI — OpenAI Codex (`plan/04-workers.md`
//! §Adapter: codex).
//!
//! The adapter builds the exact command line of the plan, drives it through
//! the shared [`crate::supervisor`], and normalises the `codex exec --json`
//! JSONL stream into [`WorkerEvent`]s:
//!
//! | `codex exec --json` line | [`WorkerEvent`] |
//! |---|---|
//! | `{"type":"thread.started","thread_id"}` | `Started{session_id}` |
//! | `item.completed` / `agent_message` | `AssistantText` |
//! | `item.completed` / `reasoning` | `Thinking` |
//! | `item.started` / tool item | `ToolCall` |
//! | `item.completed` / tool item | `ToolResult{ok}` |
//! | `{"type":"turn.completed","usage"}` | `Usage{delta}` then `Final` |
//! | `{"type":"turn.failed","error":{"message"}}` | `Failed` |
//! | `{"type":"error","message"}` | `Failed` |
//!
//! Tool items are `command_execution`, `file_change`, `mcp_tool_call`,
//! `web_search` and `todo_list`; the event's `name` is the item type verbatim.
//! Everything else (`turn.started`, `item.updated`, malformed JSON) is
//! transcript-only. Golden fixtures under `tests/fixtures/codex/` pin the
//! shapes against a real capture of `codex-cli` 0.149.0.
//!
//! Three things differ from the other adapters:
//!
//! - **No system-prompt flag.** `codex exec` has none, so the Kevin briefing is
//!   prepended to the prompt written on stdin (the argv ends with `-`).
//! - **The final answer is a file.** `-o/--output-last-message` receives the
//!   last agent message; the driver reads it after exit and falls back to the
//!   last `agent_message` item when the file is missing.
//! - **No cost.** `turn.completed.usage` has no price field at all, so
//!   `Usage::cost_usd` stays `None` and the router price table decides
//!   (`plan/04-workers.md` §Usage, cost, effort, sessions, limits).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use kevin_config::{CodexSandbox, CodexWorker as CodexConfig};
use kevin_domain::{Effort, FailureClass, ModelAlias, WorkerKind};
use serde_json::Value;

use crate::policy::SandboxPolicy;
use crate::registry::{RegistryConfig, locate_binary, probe_binary};
use crate::structured;
use crate::supervisor::{
    ChildExit, ChildHandle, SpawnOpts, Stream, Supervisor, Verdict, classify, transcript_path,
};
use crate::types::{
    ArtifactRef, ConfigError, ModelEntry, TaskAttemptRequest, Usage, WorkspacePolicy,
};
use crate::usage::parse_usage;
use crate::worker::{
    AuthStatus, Doctor, EventSink, Worker, WorkerError, WorkerEvent, WorkerHandle, WorkerOutcome,
    WorkerSessionId,
};

/// How much of a tool input/output is kept in an event summary.
pub const SUMMARY_CHARS: usize = 200;

/// Environment variables that on their own prove `codex` can authenticate.
pub const AUTH_ENV_VARS: &[&str] = &["OPENAI_API_KEY"];

/// `item.type` values that become [`WorkerEvent::ToolCall`]/[`WorkerEvent::ToolResult`].
pub const TOOL_ITEM_TYPES: &[&str] = &[
    "command_execution",
    "file_change",
    "mcp_tool_call",
    "web_search",
    "todo_list",
];

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// The `codex` adapter.
#[derive(Debug, Clone)]
pub struct CodexWorker {
    cfg: CodexConfig,
    policy: SandboxPolicy,
    kill_grace: Duration,
    data_dir: PathBuf,
    ephemeral: bool,
}

impl CodexWorker {
    /// An adapter for `[workers.codex]` under `policy`.
    pub fn new(
        cfg: CodexConfig,
        policy: SandboxPolicy,
        kill_grace: Duration,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cfg,
            policy,
            kill_grace,
            data_dir: data_dir.into(),
            ephemeral: false,
        }
    }

    /// Builds the adapter from the registry's configuration slice.
    ///
    /// `workers.codex.sandbox = "danger-full-access"` outside the `container`
    /// tier is rejected here so `kevin workers doctor` and startup fail loudly
    /// instead of at the first attempt (`plan/09-security.md`).
    pub fn from_registry_config(
        cfg: &RegistryConfig,
        policy: &SandboxPolicy,
    ) -> Result<Self, ConfigError> {
        let codex = cfg.codex.clone();
        if codex.sandbox == CodexSandbox::DangerFullAccess && !policy.allows_dangerous_flags() {
            return Err(ConfigError::Invalid {
                key: "workers.codex.sandbox".to_owned(),
                layer: kevin_config::Source::Default,
                message: format!(
                    "`danger-full-access` requires sandbox.tier = \"container\" (effective tier: `{}`)",
                    policy.tier
                ),
            });
        }
        let worker = Self::new(codex, *policy, cfg.kill_grace, cfg.data_dir.clone());
        policy
            .check_argv(&worker.cfg.extra_args)
            .map_err(|err| ConfigError::Invalid {
                key: "workers.codex.extra_args".to_owned(),
                layer: kevin_config::Source::Default,
                message: err.to_string(),
            })?;
        Ok(worker)
    }

    /// The `[workers.codex]` slice in use.
    #[must_use]
    pub const fn config(&self) -> &CodexConfig {
        &self.cfg
    }

    /// Overrides `workers.codex.bin` (the registry applies
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

    /// Adds `--ephemeral` to every invocation (no session file on disk).
    ///
    /// Off by default: `codex exec resume` needs the persisted session, which
    /// both follow-up attempts and the schema repair turn rely on.
    #[must_use]
    pub const fn with_ephemeral(mut self, ephemeral: bool) -> Self {
        self.ephemeral = ephemeral;
        self
    }

    /// Where `-o/--output-last-message` writes the final answer.
    #[must_use]
    pub fn last_message_path(&self, req: &TaskAttemptRequest) -> PathBuf {
        self.attempt_dir(req)
            .join(format!("{}.last.txt", req.attempt_id))
    }

    /// Where the `--output-schema` file is written before the spawn.
    #[must_use]
    pub fn output_schema_path(&self, req: &TaskAttemptRequest) -> PathBuf {
        self.attempt_dir(req)
            .join(format!("{}.schema.json", req.attempt_id))
    }

    fn attempt_dir(&self, req: &TaskAttemptRequest) -> PathBuf {
        self.data_dir
            .join("runs")
            .join(req.run_id.to_string())
            .join(req.task_id.to_string())
    }

    /// The complete argv (program excluded) for `req`
    /// (`plan/04-workers.md` §Adapter: codex).
    ///
    /// `resume` overrides `context.prior_session` (used by the schema repair
    /// turn, which always resumes the session of the first turn). A resumed
    /// invocation is `codex exec resume <id> …`, which — verified against
    /// `codex-cli` 0.149.0 — accepts neither `-C` nor `-s`: the working
    /// directory comes from the supervisor's `current_dir` and the sandbox
    /// from the session being resumed.
    pub fn build_argv(
        &self,
        req: &TaskAttemptRequest,
        resume: Option<&str>,
    ) -> Result<Vec<String>, WorkerError> {
        let session = resume.or_else(|| {
            req.context
                .prior_session
                .as_ref()
                .map(WorkerSessionId::as_str)
        });
        let mut argv = vec!["exec".to_owned()];
        if let Some(session) = session {
            argv.push("resume".to_owned());
            argv.push(session.to_owned());
        }
        argv.push("--json".to_owned());
        argv.push("-m".to_owned());
        argv.push(req.model.model.clone());
        if session.is_none() {
            argv.push("-C".to_owned());
            argv.push(req.workspace.root.to_string_lossy().into_owned());
            argv.push("-s".to_owned());
            argv.push(self.sandbox_mode(req).to_owned());
        }
        if req.spec.output_schema.is_some() {
            argv.push("--output-schema".to_owned());
            argv.push(self.output_schema_path(req).to_string_lossy().into_owned());
        }
        argv.push("-o".to_owned());
        argv.push(self.last_message_path(req).to_string_lossy().into_owned());
        if let Some(effort) = req.route.effort {
            argv.push("-c".to_owned());
            argv.push(format!("model_reasoning_effort={}", effort_value(effort)));
        }
        argv.push("--skip-git-repo-check".to_owned());
        if self.ephemeral {
            argv.push("--ephemeral".to_owned());
        }
        // `workers.codex.extra_args` defaults to `["--skip-git-repo-check"]`,
        // which the adapter already emits; never pass the same flag twice.
        let extra: Vec<String> = self
            .cfg
            .extra_args
            .iter()
            .filter(|a| !argv.contains(a))
            .cloned()
            .collect();
        argv.extend(extra);
        // The prompt is written to stdin (avoids argv length limits and shell
        // quoting); `-` is how `codex exec` is told to read it from there.
        argv.push("-".to_owned());
        self.policy.check_argv(&argv)?;
        Ok(argv)
    }

    /// `-s <mode>`: an in-place, read-only attempt is always `read-only`
    /// whatever `workers.codex.sandbox` says (`plan/09-security.md`).
    fn sandbox_mode(&self, req: &TaskAttemptRequest) -> &'static str {
        if req.spec.workspace_policy == WorkspacePolicy::ReadOnly {
            return "read-only";
        }
        match self.cfg.sandbox {
            CodexSandbox::ReadOnly => "read-only",
            CodexSandbox::WorkspaceWrite => "workspace-write",
            CodexSandbox::DangerFullAccess => "danger-full-access",
        }
    }

    fn spawn_opts(&self, req: &TaskAttemptRequest, prompt: &str) -> SpawnOpts {
        SpawnOpts::new(WorkerKind::Codex, req.workspace.root.clone())
            .env(req.process_env())
            .stdin(prompt.as_bytes().to_vec())
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
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<ChildHandle, WorkerError> {
        let argv = self.build_argv(req, resume)?;
        self.prepare_files(req)?;
        let mut cmd = Supervisor::command(&self.cfg.bin);
        cmd.args(&argv);
        tracing::debug!(
            kind = %WorkerKind::Codex,
            bin = %self.cfg.bin,
            model = %req.model.model,
            resume = resume.is_some(),
            "spawning codex"
        );
        Supervisor::spawn(cmd, self.spawn_opts(req, prompt))
    }

    /// Creates the attempt directory, drops a stale `-o` file and writes the
    /// `--output-schema` file when one is needed.
    fn prepare_files(&self, req: &TaskAttemptRequest) -> Result<(), WorkerError> {
        let dir = self.attempt_dir(req);
        std::fs::create_dir_all(&dir)
            .map_err(|e| WorkerError::io(format!("creating {}", dir.display()), e))?;
        let last = self.last_message_path(req);
        if last.exists() {
            std::fs::remove_file(&last)
                .map_err(|e| WorkerError::io(format!("removing {}", last.display()), e))?;
        }
        if let Some(schema) = &req.spec.output_schema {
            let path = self.output_schema_path(req);
            std::fs::write(&path, schema.to_string())
                .map_err(|e| WorkerError::io(format!("writing {}", path.display()), e))?;
        }
        Ok(())
    }

    /// The final answer of a finished turn: the `-o` file when `codex` wrote
    /// one, else the last `agent_message` item of the stream.
    fn last_message(&self, req: &TaskAttemptRequest, stream: &CodexStream) -> String {
        let path = self.last_message_path(req);
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => text.trim_end_matches('\n').to_owned(),
            Ok(_) => stream.agent_message().to_owned(),
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err,
                    "no --output-last-message file; using the last agent_message");
                stream.agent_message().to_owned()
            }
        }
    }
}

/// [`Effort`] → `-c model_reasoning_effort=<value>`.
///
/// 1:1 — `codex-cli` 0.149.0 accepts `minimal|low|medium|high|xhigh|max|ultra`,
/// so `Max` maps to `max` rather than to `high` as `plan/04-workers.md`
/// originally guessed.
#[must_use]
pub const fn effort_value(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::XHigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The Kevin briefing prepended to the stdin prompt: task title, acceptance
/// criteria, operator/lesson context and the memory block. `codex exec` has no
/// `--append-system-prompt`, so it travels with the prompt.
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

/// What is written to the child's stdin: briefing then instructions.
#[must_use]
pub fn prompt_of(req: &TaskAttemptRequest) -> String {
    let body = if req.spec.instructions.trim().is_empty() {
        req.prompt_text()
    } else {
        req.spec.instructions.clone()
    };
    let briefing = briefing(req);
    if briefing.is_empty() {
        body
    } else {
        format!("{briefing}\n\n# Instructions\n{body}")
    }
}

#[async_trait]
impl Worker for CodexWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Codex
    }

    async fn doctor(&self) -> Doctor {
        let mut doctor = probe_binary(WorkerKind::Codex, &self.cfg.bin).await;
        if doctor.binary.is_some() {
            doctor.auth_ready = auth_status();
            if let AuthStatus::Missing(hint) = &doctor.auth_ready {
                doctor.notes.push(format!("auth: {hint}"));
            }
        }
        doctor
    }

    fn validate_alias(&self, alias: &ModelAlias, entry: &ModelEntry) -> Result<(), ConfigError> {
        if entry.worker != WorkerKind::Codex {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                format!("worker: expected `codex`, found `{}`", entry.worker),
            ));
        }
        if entry.model.trim().is_empty() {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                "model: must be a non-empty Codex model id (e.g. `gpt-5.6`)",
            ));
        }
        if let Some(key) = entry.extra.keys().next() {
            return Err(ConfigError::invalid_model_entry(
                alias.clone(),
                format!("unknown key `{key}`: the codex worker takes no extra model keys"),
            ));
        }
        Ok(())
    }

    async fn start(&self, req: TaskAttemptRequest) -> Result<WorkerHandle, WorkerError> {
        if locate_binary(&self.cfg.bin).is_none() {
            return Err(WorkerError::BinaryMissing {
                kind: WorkerKind::Codex,
                bin: self.cfg.bin.clone(),
            });
        }
        // Fail fast on policy violations and an unusable workspace: `start`
        // may only return `Err` for things that prevent spawning at all.
        self.build_argv(&req, None)?;
        if !req.workspace.root.is_dir() {
            return Err(WorkerError::WorkspaceUnavailable {
                path: req.workspace.root.clone(),
                reason: "not a directory".to_owned(),
            });
        }
        let worker = self.clone();
        let cancel = req.cancel.clone();
        Ok(WorkerHandle::spawn(
            WorkerKind::Codex,
            cancel,
            move |sink| async move { worker.drive(req, sink).await },
        ))
    }
}

/// Credentials check that never calls the API (`plan/04-workers.md`
/// §Registry and doctor): `$CODEX_HOME/auth.json` or `OPENAI_API_KEY`.
fn auth_status() -> AuthStatus {
    for name in AUTH_ENV_VARS {
        if std::env::var(name).is_ok_and(|v| !v.trim().is_empty()) {
            return AuthStatus::Ready;
        }
    }
    if let Some(home) = codex_home()
        && home.join("auth.json").is_file()
    {
        return AuthStatus::Ready;
    }
    AuthStatus::Missing("run `codex login`, or set OPENAI_API_KEY".to_owned())
}

/// `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// What one `codex exec` invocation produced.
struct Turn {
    stream: CodexStream,
    exit: ChildExit,
    /// `-o` file contents, or the last `agent_message` as a fallback.
    text: String,
}

impl CodexWorker {
    /// Runs the attempt: one turn, plus at most one schema repair turn.
    async fn drive(self, req: TaskAttemptRequest, mut sink: EventSink) -> WorkerOutcome {
        let prompt = prompt_of(&req);
        let mut turn = match self.run_turn(&req, &mut sink, &prompt, None).await {
            Ok(turn) => turn,
            Err(err) => return fail(&mut sink, FailureClass::Transient, err.to_string()).await,
        };
        let mut transcript = turn.exit.transcript.clone();
        let mut structured = None;

        if let Some(schema) = req.spec.output_schema.clone()
            && matches!(
                classify(&turn.exit, turn.stream.saw_final()),
                Verdict::Succeeded
            )
        {
            let mut err = match structured::extract_and_validate(&turn.text, &schema) {
                Ok(value) => {
                    structured = Some(value);
                    None
                }
                Err(err) => Some(err),
            };
            // One repair turn on the same session (`plan/04` §Structured output).
            if let (Some(violation), Some(session)) = (&err, turn.stream.session_id.clone()) {
                tracing::debug!(error = %violation, "codex answer failed schema validation; repairing");
                let prompt = structured::repair_prompt(violation);
                match self
                    .run_turn(&req, &mut sink, &prompt, Some(session.as_str()))
                    .await
                {
                    Ok(mut repair) => {
                        transcript = repair.exit.transcript.clone().or(transcript);
                        err = match structured::extract_and_validate(&repair.text, &schema) {
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

    /// Spawns one `codex exec`, streams its stdout into `sink` and returns the
    /// parsed stream plus the child's exit report. Terminal events are held
    /// back: the driver emits exactly one, after exit classification.
    async fn run_turn(
        &self,
        req: &TaskAttemptRequest,
        sink: &mut EventSink,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<Turn, WorkerError> {
        let mut child = self.spawn(req, prompt, resume)?;
        let mut stream = CodexStream::new(Some(child.pid()));
        if resume.is_some() {
            // A resumed turn never re-emits `Started`.
            stream.started = true;
            stream.session_id = resume.map(WorkerSessionId::new);
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
        let text = self.last_message(req, &stream);
        Ok(Turn { stream, exit, text })
    }

    /// Emits the single terminal event and returns the matching outcome.
    async fn finish(
        &self,
        turn: Turn,
        structured: Option<Value>,
        transcript: Option<ArtifactRef>,
        sink: &mut EventSink,
    ) -> WorkerOutcome {
        let Turn { stream, exit, text } = turn;
        let usage = stream.finish_usage(&exit);
        let verdict = match &stream.terminal {
            // The CLI reported a failure itself: keep its class/message even
            // when the process exited 0.
            Some(Terminal::Failed { class, message }) if exit.success() => Verdict::Failed {
                class: *class,
                message: message.clone(),
            },
            _ => classify(&exit, stream.saw_final()),
        };
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
// JSONL parser
// ---------------------------------------------------------------------------

/// The terminal line of a stream (`turn.completed`, `turn.failed`, `error`).
#[derive(Debug, Clone, PartialEq)]
enum Terminal {
    Final,
    Failed {
        class: FailureClass,
        message: String,
    },
}

/// Incremental parser of `codex exec --json` output.
///
/// One instance per `codex exec` invocation. [`CodexStream::parse_line`] never
/// panics: anything it does not understand is counted and ignored (it still
/// reaches the transcript, which the supervisor writes from the raw pipes).
#[derive(Debug, Default)]
pub struct CodexStream {
    pid: Option<u32>,
    started: bool,
    session_id: Option<WorkerSessionId>,
    /// Tool items already announced with a `ToolCall` (`item.id` → name).
    open_tools: HashMap<String, String>,
    usage: Usage,
    /// Text of the last `agent_message` item (fallback for the `-o` file).
    agent_message: String,
    /// Message of the last `error` *item*, used when the turn fails mutely.
    error_note: Option<String>,
    terminal: Option<Terminal>,
    malformed: u64,
}

impl CodexStream {
    /// A parser for a child with this pid.
    #[must_use]
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            ..Self::default()
        }
    }

    /// Session id (`thread.started.thread_id`), once seen.
    #[must_use]
    pub const fn session_id(&self) -> Option<&WorkerSessionId> {
        self.session_id.as_ref()
    }

    /// Usage accumulated so far (`turn.completed.usage`).
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Text of the last `agent_message` item.
    #[must_use]
    pub fn agent_message(&self) -> &str {
        &self.agent_message
    }

    /// `true` once a `turn.completed` line was seen.
    #[must_use]
    pub fn saw_final(&self) -> bool {
        matches!(self.terminal, Some(Terminal::Final))
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

    /// Parses one stdout line into zero or more events.
    #[must_use]
    pub fn parse_line(&mut self, line: &str) -> Vec<WorkerEvent> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            self.malformed += 1;
            tracing::debug!(len = trimmed.len(), "unparsable codex exec --json line");
            return Vec::new();
        };
        if !value.is_object() {
            self.malformed += 1;
            return Vec::new();
        }
        match str_at(&value, "type") {
            Some("thread.started") => self.parse_thread_started(&value),
            Some("item.started") => self.parse_item(&value, false),
            Some("item.completed") => self.parse_item(&value, true),
            Some("turn.completed") => self.parse_turn_completed(&value),
            Some("turn.failed") => {
                let message = value
                    .get("error")
                    .and_then(|e| str_at(e, "message"))
                    .or(self.error_note.as_deref())
                    .unwrap_or("turn failed")
                    .to_owned();
                self.fail(format!("codex turn.failed: {}", truncate(&message, 512)))
            }
            Some("error") => {
                let message = str_at(&value, "message").unwrap_or("error").to_owned();
                self.fail(format!("codex error: {}", truncate(&message, 512)))
            }
            // `turn.started`, `item.updated` and future variants are
            // transcript-only.
            _ => Vec::new(),
        }
    }

    fn parse_thread_started(&mut self, value: &Value) -> Vec<WorkerEvent> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        self.session_id = str_at(value, "thread_id").map(WorkerSessionId::new);
        vec![WorkerEvent::Started {
            session_id: self.session_id.clone(),
            pid: self.pid,
        }]
    }

    /// `item.started` / `item.completed`.
    fn parse_item(&mut self, value: &Value, completed: bool) -> Vec<WorkerEvent> {
        let Some(item) = value.get("item") else {
            return Vec::new();
        };
        let Some(item_type) = str_at(item, "type") else {
            return Vec::new();
        };
        match item_type {
            "agent_message" => {
                if !completed {
                    return Vec::new();
                }
                let text = str_at(item, "text").unwrap_or_default();
                if text.is_empty() {
                    return Vec::new();
                }
                text.clone_into(&mut self.agent_message);
                vec![WorkerEvent::AssistantText {
                    delta: text.to_owned(),
                }]
            }
            "reasoning" => {
                if !completed {
                    return Vec::new();
                }
                match str_at(item, "text").filter(|t| !t.is_empty()) {
                    Some(text) => vec![WorkerEvent::Thinking {
                        delta: text.to_owned(),
                    }],
                    None => Vec::new(),
                }
            }
            "error" => {
                if completed {
                    self.error_note = str_at(item, "message").map(str::to_owned);
                }
                Vec::new()
            }
            t if TOOL_ITEM_TYPES.contains(&t) => self.parse_tool_item(item, t, completed),
            _ => Vec::new(),
        }
    }

    fn parse_tool_item(&mut self, item: &Value, name: &str, completed: bool) -> Vec<WorkerEvent> {
        let id = str_at(item, "id").unwrap_or_default().to_owned();
        let mut events = Vec::new();
        // An item that completes without ever being announced (fast tools only
        // emit `item.completed`) still gets its `ToolCall`.
        if !self.open_tools.contains_key(&id) {
            self.open_tools.insert(id.clone(), name.to_owned());
            events.push(WorkerEvent::ToolCall {
                name: name.to_owned(),
                input_summary: truncate(&tool_input(item, name), SUMMARY_CHARS),
            });
        }
        if completed {
            self.open_tools.remove(&id);
            events.push(WorkerEvent::ToolResult {
                name: name.to_owned(),
                ok: tool_ok(item),
                output_summary: truncate(&tool_output(item, name), SUMMARY_CHARS),
            });
        }
        events
    }

    fn parse_turn_completed(&mut self, value: &Value) -> Vec<WorkerEvent> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        self.terminal = Some(Terminal::Final);
        let total = value.get("usage").map(codex_usage).unwrap_or_default();
        let delta = Usage {
            input_tokens: total.input_tokens.saturating_sub(self.usage.input_tokens),
            output_tokens: total.output_tokens.saturating_sub(self.usage.output_tokens),
            cache_read_tokens: total
                .cache_read_tokens
                .saturating_sub(self.usage.cache_read_tokens),
            cache_write_tokens: total
                .cache_write_tokens
                .saturating_sub(self.usage.cache_write_tokens),
            cost_usd: None,
            wall_ms: 0,
        };
        self.usage = total;
        let mut events = Vec::new();
        if !delta.is_empty() {
            events.push(WorkerEvent::Usage { delta });
        }
        events.push(WorkerEvent::Final {
            text: self.agent_message.clone(),
            structured: None,
            usage: self.usage.clone(),
        });
        events
    }

    fn fail(&mut self, message: String) -> Vec<WorkerEvent> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        let class = failure_class(&message);
        self.terminal = Some(Terminal::Failed {
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

/// `turn.completed.usage` → [`Usage`] (`plan/04-workers.md` §Usage).
///
/// Codex reports `input_tokens` as the *total* prompt size with
/// `cached_input_tokens` as the cached subset; Kevin's [`Usage`] keeps the two
/// disjoint (`total_tokens()` must not double count), so the cached part is
/// subtracted. `reasoning_output_tokens` is already included in
/// `output_tokens` and is therefore not added again. There is no cost field —
/// `cost_usd` stays `None` and the router price table decides.
#[must_use]
pub fn codex_usage(value: &Value) -> Usage {
    let mut usage = parse_usage(value);
    if usage.cache_write_tokens == 0 {
        usage.cache_write_tokens = value
            .get("cache_write_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
    usage.input_tokens = usage.input_tokens.saturating_sub(usage.cache_read_tokens);
    usage.cost_usd = None;
    usage
}

/// Failure class of a `turn.failed` / `error` message.
#[must_use]
pub fn failure_class(message: &str) -> FailureClass {
    if crate::supervisor::is_transient_signature(message) {
        FailureClass::Transient
    } else {
        FailureClass::Permanent
    }
}

/// The `input_summary` of a tool item.
fn tool_input(item: &Value, item_type: &str) -> String {
    match item_type {
        "command_execution" => str_at(item, "command").unwrap_or_default().to_owned(),
        "web_search" => str_at(item, "query").unwrap_or_default().to_owned(),
        "file_change" => item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .map(|c| {
                        format!(
                            "{} {}",
                            str_at(c, "kind").unwrap_or("update"),
                            str_at(c, "path").unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "mcp_tool_call" => format!(
            "{}/{} {}",
            str_at(item, "server").unwrap_or_default(),
            str_at(item, "tool").unwrap_or_default(),
            json_text(item.get("arguments"))
        )
        .trim()
        .to_owned(),
        _ => json_text(item.get("items")),
    }
}

/// The `output_summary` of a completed tool item.
fn tool_output(item: &Value, item_type: &str) -> String {
    match item_type {
        "command_execution" => str_at(item, "aggregated_output")
            .unwrap_or_default()
            .to_owned(),
        "mcp_tool_call" => json_text(item.get("result")),
        _ => str_at(item, "status").unwrap_or_default().to_owned(),
    }
}

/// `ok` of a completed tool item: a non-`completed` status or a non-zero
/// `exit_code` is a failure.
fn tool_ok(item: &Value) -> bool {
    if item
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
    {
        return false;
    }
    !matches!(str_at(item, "status"), Some("failed" | "declined"))
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A JSON value as text; strings keep their contents, everything else is
/// re-serialised.
fn json_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
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
                worker: WorkerKind::Codex,
                model: ModelAlias::new("gpt56-codex").unwrap(),
                effort: None,
            },
            model: ModelEntry::new(WorkerKind::Codex, "gpt-5.6"),
            workspace: Workspace::in_place("/workspace"),
            context: AttemptContext::default(),
            env: EnvAllowlist::new(["PATH"]),
            budget: AttemptBudget::default(),
            cancel: CancellationToken::new(),
        }
    }

    fn worker() -> CodexWorker {
        CodexWorker::new(
            CodexConfig::default(),
            SandboxPolicy::cli_native(),
            Duration::from_secs(10),
            "/data",
        )
    }

    #[test]
    fn argv_matches_the_plan() {
        let w = worker();
        let r = req();
        let argv = w.build_argv(&r, None).unwrap();
        assert_eq!(
            argv.join(" "),
            format!(
                "exec --json -m gpt-5.6 -C /workspace -s workspace-write -o {} \
                 --skip-git-repo-check -",
                w.last_message_path(&r).display()
            )
        );
        // `--skip-git-repo-check` also sits in the default extra_args and is
        // emitted exactly once.
        assert_eq!(
            argv.iter()
                .filter(|a| *a == "--skip-git-repo-check")
                .count(),
            1
        );
    }

    #[test]
    fn argv_switches_to_resume_schema_effort_and_read_only() {
        let w = worker();
        let mut r = req();
        r.spec.output_schema = Some(json!({"type": "object"}));
        r.spec.workspace_policy = WorkspacePolicy::ReadOnly;
        r.route.effort = Some(Effort::Max);
        let argv = w.build_argv(&r, None).unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("-s read-only"), "{joined}");
        assert!(joined.contains("-c model_reasoning_effort=max"), "{joined}");
        assert!(
            joined.contains(&format!(
                "--output-schema {}",
                w.output_schema_path(&r).display()
            )),
            "{joined}"
        );

        // Resume drops `-C`/`-s` (unsupported by `codex exec resume`).
        r.context.prior_session = Some(WorkerSessionId::new("sess-1"));
        let argv = w.build_argv(&r, None).unwrap();
        assert_eq!(argv[..3], ["exec", "resume", "sess-1"]);
        assert!(!argv.iter().any(|a| a == "-C" || a == "-s"));
        assert_eq!(argv.last().unwrap(), "-");
        // An explicit resume target wins over `context.prior_session`.
        let argv = w.build_argv(&r, Some("sess-2")).unwrap();
        assert_eq!(argv[2], "sess-2");
    }

    #[test]
    fn ephemeral_is_opt_in() {
        assert!(
            !worker()
                .build_argv(&req(), None)
                .unwrap()
                .contains(&"--ephemeral".to_owned())
        );
        assert!(
            worker()
                .with_ephemeral(true)
                .build_argv(&req(), None)
                .unwrap()
                .contains(&"--ephemeral".to_owned())
        );
    }

    #[test]
    fn danger_full_access_is_a_policy_violation_outside_container() {
        let cfg = CodexConfig {
            sandbox: CodexSandbox::DangerFullAccess,
            ..CodexConfig::default()
        };
        let native = CodexWorker::new(
            cfg.clone(),
            SandboxPolicy::cli_native(),
            Duration::from_secs(1),
            "/data",
        );
        let err = native.build_argv(&req(), None).unwrap_err();
        assert!(
            matches!(&err, WorkerError::PolicyViolation { flag, .. } if flag == "danger-full-access"),
            "{err}"
        );
        let container = CodexWorker::new(
            cfg,
            SandboxPolicy::container(),
            Duration::from_secs(1),
            "/data",
        );
        assert!(
            container
                .build_argv(&req(), None)
                .unwrap()
                .join(" ")
                .contains("-s danger-full-access")
        );
    }

    #[test]
    fn bypass_extra_args_are_rejected_outside_container() {
        let cfg = CodexConfig {
            extra_args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
            ..CodexConfig::default()
        };
        let worker = CodexWorker::new(
            cfg,
            SandboxPolicy::cli_native(),
            Duration::from_secs(1),
            "/data",
        );
        assert!(matches!(
            worker.build_argv(&req(), None),
            Err(WorkerError::PolicyViolation { .. })
        ));
    }

    #[test]
    fn prompt_carries_the_briefing() {
        let mut r = req();
        r.spec.acceptance_criteria = vec!["tests pass".into()];
        r.context.memory = Some("<kevin-memory>lesson</kevin-memory>".into());
        let prompt = prompt_of(&r);
        assert!(prompt.contains("# Kevin task\nAdd auth (kind: implement)"));
        assert!(prompt.contains("- tests pass"));
        assert!(prompt.contains("<kevin-memory>"));
        assert!(prompt.ends_with("# Instructions\nImplement the login flow."));
    }

    #[test]
    fn parser_maps_the_documented_lines() {
        let mut s = CodexStream::new(Some(42));
        assert!(s.parse_line("").is_empty());
        assert!(s.parse_line("not json").is_empty());
        assert_eq!(s.malformed_lines(), 1);
        assert!(s.parse_line(r#"{"type":"turn.started"}"#).is_empty());

        let ev = s.parse_line(r#"{"type":"thread.started","thread_id":"th-1"}"#);
        assert_eq!(
            ev,
            vec![WorkerEvent::Started {
                session_id: Some(WorkerSessionId::new("th-1")),
                pid: Some(42)
            }]
        );
        assert_eq!(
            s.parse_line(
                &json!({"type":"item.completed",
                    "item":{"id":"i0","type":"reasoning","text":"hmm"}})
                .to_string()
            ),
            vec![WorkerEvent::Thinking {
                delta: "hmm".into()
            }]
        );
        assert_eq!(
            s.parse_line(
                &json!({"type":"item.started",
                    "item":{"id":"i1","type":"command_execution","command":"ls",
                            "aggregated_output":"","exit_code":null,"status":"in_progress"}})
                .to_string()
            ),
            vec![WorkerEvent::ToolCall {
                name: "command_execution".into(),
                input_summary: "ls".into()
            }]
        );
        assert_eq!(
            s.parse_line(
                &json!({"type":"item.completed",
                    "item":{"id":"i1","type":"command_execution","command":"ls",
                            "aggregated_output":"main.rs\n","exit_code":0,"status":"completed"}})
                .to_string()
            ),
            vec![WorkerEvent::ToolResult {
                name: "command_execution".into(),
                ok: true,
                output_summary: "main.rs\n".into()
            }]
        );
        assert_eq!(
            s.parse_line(
                &json!({"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"hi"}})
                    .to_string()
            ),
            vec![WorkerEvent::AssistantText { delta: "hi".into() }]
        );
        assert_eq!(s.agent_message(), "hi");
        assert!(!s.saw_final());

        let ev = s.parse_line(
            &json!({"type":"turn.completed","usage":{"input_tokens":100,
                "cached_input_tokens":40,"cache_write_input_tokens":5,
                "output_tokens":20,"reasoning_output_tokens":8}})
            .to_string(),
        );
        assert_eq!(
            ev[0],
            WorkerEvent::Usage {
                delta: Usage {
                    input_tokens: 60,
                    output_tokens: 20,
                    cache_read_tokens: 40,
                    cache_write_tokens: 5,
                    cost_usd: None,
                    wall_ms: 0,
                }
            }
        );
        assert!(matches!(&ev[1], WorkerEvent::Final { text, usage, .. }
            if text == "hi" && usage.cost_usd.is_none()));
        assert!(s.saw_final());
        // Only the first terminal line counts.
        assert!(s.parse_line(r#"{"type":"turn.failed"}"#).is_empty());
    }

    #[test]
    fn a_completed_only_tool_item_still_gets_a_tool_call() {
        let mut s = CodexStream::new(None);
        let ev = s.parse_line(
            &json!({"type":"item.completed","item":{"id":"i0","type":"file_change",
                "changes":[{"path":"a.rs","kind":"update"}],"status":"completed"}})
            .to_string(),
        );
        assert_eq!(
            ev,
            vec![
                WorkerEvent::ToolCall {
                    name: "file_change".into(),
                    input_summary: "update a.rs".into()
                },
                WorkerEvent::ToolResult {
                    name: "file_change".into(),
                    ok: true,
                    output_summary: "completed".into()
                }
            ]
        );
    }

    #[test]
    fn turn_failed_and_error_lines_map_to_failure_classes() {
        let mut s = CodexStream::new(None);
        let ev = s.parse_line(
            &json!({"type":"turn.failed","error":{"message":"tool call budget exhausted"}})
                .to_string(),
        );
        assert!(matches!(&ev[0],
            WorkerEvent::Failed { class: FailureClass::Permanent, message, .. }
                if message.contains("tool call budget")));

        let mut s = CodexStream::new(None);
        let ev =
            s.parse_line(&json!({"type":"error","message":"429 Too Many Requests"}).to_string());
        assert!(matches!(
            &ev[0],
            WorkerEvent::Failed {
                class: FailureClass::Transient,
                ..
            }
        ));
        assert!(!s.saw_final());

        // A mute `turn.failed` reuses the last `error` item message.
        let mut s = CodexStream::new(None);
        let _ = s.parse_line(
            &json!({"type":"item.completed","item":{"id":"i0","type":"error",
                "message":"connection reset by peer"}})
            .to_string(),
        );
        let ev = s.parse_line(r#"{"type":"turn.failed"}"#);
        assert!(matches!(
            &ev[0],
            WorkerEvent::Failed {
                class: FailureClass::Transient,
                ..
            }
        ));
    }

    #[test]
    fn helpers_are_char_safe() {
        assert_eq!(truncate("héllo", 3), "hél…");
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(json_text(None), "");
        assert_eq!(json_text(Some(&json!("a"))), "a");
        assert_eq!(json_text(Some(&json!({"a": 1}))), r#"{"a":1}"#);
        assert_eq!(effort_value(Effort::XHigh), "xhigh");
        assert!(!tool_ok(&json!({"status": "declined"})));
        assert!(!tool_ok(&json!({"exit_code": 1, "status": "completed"})));
        assert!(tool_ok(&json!({"exit_code": 0})));
    }

    #[tokio::test]
    async fn validate_alias_rejects_foreign_workers_and_extras() {
        let w = worker();
        let alias = ModelAlias::new("gpt56-codex").unwrap();
        assert!(
            w.validate_alias(&alias, &ModelEntry::new(WorkerKind::Codex, "gpt-5.6"))
                .is_ok()
        );
        assert!(
            w.validate_alias(
                &alias,
                &ModelEntry::new(WorkerKind::Claude, "claude-sonnet-5")
            )
            .is_err()
        );
        assert!(
            w.validate_alias(&alias, &ModelEntry::new(WorkerKind::Codex, "  "))
                .is_err()
        );
        let mut entry = ModelEntry::new(WorkerKind::Codex, "gpt-5.6");
        entry
            .extra
            .insert("provider".to_owned(), toml::Value::String("openai".into()));
        assert!(w.validate_alias(&alias, &entry).is_err());
    }

    #[tokio::test]
    async fn start_reports_a_missing_binary() {
        let cfg = CodexConfig {
            bin: "definitely-not-codex-kevin".to_owned(),
            ..CodexConfig::default()
        };
        let worker = CodexWorker::new(
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
        assert_eq!(doctor.kind, WorkerKind::Codex);
    }
}
