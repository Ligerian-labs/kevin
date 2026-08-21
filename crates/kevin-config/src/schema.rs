//! The typed configuration schema (`plan/03-config-schema.md` §Full schema).
//!
//! Every section is a struct with `#[serde(deny_unknown_fields, default)]` so
//! partial files/layers deserialize against the defaults and any typo is an
//! error. `KevinConfig::default()` is, field for field, the TOML block in the
//! plan (embedded as [`crate::DEFAULT_TOML`] and checked by a test).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use kevin_domain::{Effort, ModelAlias, TaskKind, WorkerKind};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::duration;

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// USD amount from an integer number of cents (`usd_cents(1000)` = `10.00`).
fn usd_cents(cents: i64) -> Decimal {
    Decimal::new(cents, 2)
}

fn alias(s: &str) -> ModelAlias {
    ModelAlias::new(s).unwrap_or_else(|e| unreachable!("built-in alias is valid: {e}"))
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

/// The whole Kevin configuration; immutable for the process lifetime once
/// loaded by [`crate::load`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KevinConfig {
    /// `[kevin]` — instance-level settings.
    pub kevin: General,
    /// `[database]` — Postgres connection.
    pub database: Database,
    /// `[server]` — HTTP API listener.
    pub server: Server,
    /// `[client]` — how the CLI/TUI reach a server.
    pub client: Client,
    /// `[budget]` — default run/task budgets and global bulkheads.
    pub budget: Budget,
    /// `[orchestrator]` — planning/questions/evaluation knobs.
    pub orchestrator: Orchestrator,
    /// `[concurrency]` — runtime threads and per-worker-kind limits.
    pub concurrency: Concurrency,
    /// `[retention]` — pruning horizons for logs, transcripts, artifacts.
    pub retention: Retention,
    /// `[workers]` — one table per worker adapter.
    pub workers: Workers,
    /// `[models.<alias>]` — the model catalog (routing vocabulary).
    pub models: BTreeMap<ModelAlias, ModelEntry>,
    /// `[roles]` — which alias fulfils each orchestration role.
    pub roles: Roles,
    /// `[routing]` — selection policy and candidates per task kind.
    pub routing: Routing,
    /// `[memory]` — pgvector memory and embeddings.
    pub memory: Memory,
    /// `[evaluation]` — judge pass and auto-apply policy.
    pub evaluation: Evaluation,
    /// `[workspace]` — task workspace isolation and integration.
    pub workspace: WorkspaceCfg,
    /// `[checks]` — repo checks run by the integrator.
    pub checks: Checks,
    /// `[sandbox]` — sandbox tier and environment allow-list.
    pub sandbox: Sandbox,
    /// `[telemetry]` — logs, metrics, traces.
    pub telemetry: Telemetry,
    /// `[kohral]` — Kohral runtime adapter.
    pub kohral: Kohral,
}

impl Default for KevinConfig {
    fn default() -> Self {
        Self {
            kevin: General::default(),
            database: Database::default(),
            server: Server::default(),
            client: Client::default(),
            budget: Budget::default(),
            orchestrator: Orchestrator::default(),
            concurrency: Concurrency::default(),
            retention: Retention::default(),
            workers: Workers::default(),
            models: default_models(),
            roles: Roles::default(),
            routing: Routing::default(),
            memory: Memory::default(),
            evaluation: Evaluation::default(),
            workspace: WorkspaceCfg::default(),
            checks: Checks::default(),
            sandbox: Sandbox::default(),
            telemetry: Telemetry::default(),
            kohral: Kohral::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// [kevin]
// ---------------------------------------------------------------------------

/// `[kevin]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    /// Artifacts, worker transcripts, embeddings cache.
    pub data_dir: PathBuf,
    /// Appears in logs/metrics; Kohral sets the agent name.
    pub instance_name: String,
    /// Only changes *defaults*, never behaviour branches.
    pub profile: Profile,
    /// `true`: skip plan approval (forced true in headless/kohral runs).
    pub auto_approve_plans: bool,
    /// Drain window on SIGTERM.
    #[serde(with = "duration")]
    pub shutdown_grace_period: Duration,
}

impl Default for General {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("~/.local/share/kevin"),
            instance_name: "kevin".into(),
            profile: Profile::Laptop,
            auto_approve_plans: false,
            shutdown_grace_period: secs(30),
        }
    }
}

/// Deployment profile; selects defaults only (`plan/03` §Validation rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Interactive use on a developer machine (the built-in defaults).
    Laptop,
    /// Daemon on a VPS: no auto-migrate, JSON logs, no Swagger UI.
    Server,
    /// Inside a Kohral stack: `server` + Kohral adapter on, plans auto-approved.
    Kohral,
}

impl Profile {
    /// Every profile.
    pub const ALL: [Profile; 3] = [Profile::Laptop, Profile::Server, Profile::Kohral];

    /// Lowercase name, identical to the config form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Profile::Laptop => "laptop",
            Profile::Server => "server",
            Profile::Kohral => "kohral",
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Profile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Profile::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unknown profile {s:?}: expected laptop, server or kohral"))
    }
}

// ---------------------------------------------------------------------------
// [database]
// ---------------------------------------------------------------------------

/// `[database]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Database {
    /// `postgres://…` connection URL (`KEVIN__DATABASE__URL`). Empty when `url_file` is used.
    pub url: String,
    /// File holding the connection URL (secret mount); alternative to `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url_file: Option<PathBuf>,
    /// Connection pool size.
    pub pool_size: u32,
    /// Run migrations on `serve` start (server/kohral profiles default false).
    pub auto_migrate: bool,
    /// Per-statement timeout.
    #[serde(with = "duration")]
    pub statement_timeout: Duration,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: "postgres://kevin:kevin@localhost:5432/kevin".into(),
            url_file: None,
            pool_size: 10,
            auto_migrate: true,
            statement_timeout: secs(30),
        }
    }
}

// ---------------------------------------------------------------------------
// [server]
// ---------------------------------------------------------------------------

/// `[server]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// `kevin run` starts an ephemeral server when no server is configured.
    pub enabled: bool,
    /// Listen address.
    pub bind: SocketAddr,
    /// Bearer token file (created by `kevin config init`; Kohral mounts its own).
    pub auth_token_file: PathBuf,
    /// Allowed CORS origins (empty = CORS disabled).
    pub cors_origins: Vec<String>,
    /// Per-request timeout.
    #[serde(with = "duration")]
    pub request_timeout: Duration,
    /// SSE keep-alive interval.
    #[serde(with = "duration")]
    pub sse_keepalive: Duration,
    /// Swagger UI at `/api/v1/docs`.
    pub docs: bool,
    /// Old token accepted this long after `rotate-token` + SIGHUP.
    #[serde(with = "duration")]
    pub token_grace: Duration,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7777),
            auth_token_file: PathBuf::from("~/.config/kevin/token"),
            cors_origins: Vec::new(),
            request_timeout: secs(30),
            sse_keepalive: secs(15),
            docs: true,
            token_grace: secs(5 * 60),
        }
    }
}

// ---------------------------------------------------------------------------
// [client]
// ---------------------------------------------------------------------------

/// `[client]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Client {
    /// Empty → embedded runtime; set to use a remote daemon.
    pub server_url: String,
    /// Bearer token file for `server_url`.
    pub token_file: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            token_file: PathBuf::from("~/.config/kevin/token"),
        }
    }
}

// ---------------------------------------------------------------------------
// [budget]
// ---------------------------------------------------------------------------

/// `[budget]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Budget {
    /// Default USD cap per run.
    #[serde(with = "rust_decimal::serde::float")]
    pub default_run_usd: Decimal,
    /// Default USD cap per task.
    #[serde(with = "rust_decimal::serde::float")]
    pub default_task_usd: Decimal,
    /// Default wall-clock cap per run.
    #[serde(with = "duration")]
    pub default_run_wall: Duration,
    /// Default wall-clock cap per task.
    #[serde(with = "duration")]
    pub default_task_wall: Duration,
    /// Attempts per task before it fails.
    pub max_attempts: u8,
    /// Global bulkhead for worker subprocesses.
    pub max_parallel_tasks: u16,
    /// Soft cap on input+output tokens per task.
    pub max_tokens_per_task: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            default_run_usd: usd_cents(1000),
            default_task_usd: usd_cents(300),
            default_run_wall: secs(2 * 3600),
            default_task_wall: secs(30 * 60),
            max_attempts: 2,
            max_parallel_tasks: 4,
            max_tokens_per_task: 2_000_000,
        }
    }
}

// ---------------------------------------------------------------------------
// [orchestrator]
// ---------------------------------------------------------------------------

/// `[orchestrator]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Orchestrator {
    /// Proposed questions below this confidence become real Questions.
    pub question_confidence_threshold: f64,
    /// Cap on questions per run.
    pub max_questions_per_run: u32,
    /// Cap on tasks per run.
    pub max_tasks_per_run: u32,
    /// Planner / judge / integrator worker call timeout.
    #[serde(with = "duration")]
    pub role_call_timeout: Duration,
    /// Headless/Kohral: apply the default answer after this; interactive: block.
    #[serde(with = "duration")]
    pub question_default_timeout: Duration,
    /// `RejectPlan` → re-plan cycles before failing.
    pub plan_revision_limit: u32,
    /// Run completes with evaluation skipped after this.
    #[serde(with = "duration")]
    pub evaluation_timeout: Duration,
    /// Minimum interval between `task.progressed` events per attempt.
    #[serde(with = "duration")]
    pub progress_interval: Duration,
}

impl Default for Orchestrator {
    fn default() -> Self {
        Self {
            question_confidence_threshold: 0.7,
            max_questions_per_run: 4,
            max_tasks_per_run: 24,
            role_call_timeout: secs(15 * 60),
            question_default_timeout: secs(10 * 60),
            plan_revision_limit: 2,
            evaluation_timeout: secs(10 * 60),
            progress_interval: secs(10),
        }
    }
}

// ---------------------------------------------------------------------------
// [concurrency]
// ---------------------------------------------------------------------------

/// `[concurrency]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Concurrency {
    /// Tokio worker threads; `0` = number of CPUs.
    pub worker_threads: u32,
    /// Concurrent subprocesses per worker kind.
    pub per_worker_kind: BTreeMap<WorkerKind, u32>,
    /// Blocking threads (embeddings etc.).
    pub blocking_threads: u32,
}

impl Default for Concurrency {
    fn default() -> Self {
        Self {
            worker_threads: 0,
            per_worker_kind: [
                (WorkerKind::Claude, 4),
                (WorkerKind::Codex, 4),
                (WorkerKind::Pi, 4),
                (WorkerKind::Opencode, 4),
                (WorkerKind::Fake, 64),
            ]
            .into_iter()
            .collect(),
            blocking_threads: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// [retention]
// ---------------------------------------------------------------------------

/// `[retention]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    /// `orch.task_log` rows (`kevin db prune`).
    pub task_log_days: u32,
    /// Raw worker transcripts under `data_dir`.
    pub transcript_days: u32,
    /// Artifacts under `data_dir`.
    pub artifact_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            task_log_days: 30,
            transcript_days: 30,
            artifact_days: 90,
        }
    }
}

// ---------------------------------------------------------------------------
// [workers]
// ---------------------------------------------------------------------------

/// `[workers]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Workers {
    /// SIGTERM → SIGKILL delay on cancel/timeout (all adapters).
    #[serde(with = "duration")]
    pub kill_grace: Duration,
    /// `[workers.claude]`.
    pub claude: ClaudeWorker,
    /// `[workers.codex]`.
    pub codex: CodexWorker,
    /// `[workers.pi]`.
    pub pi: PiWorker,
    /// `[workers.opencode]`.
    pub opencode: OpencodeWorker,
    /// `[workers.fake]`.
    pub fake: FakeWorker,
}

impl Workers {
    /// Whether the worker of `kind` is enabled.
    #[must_use]
    pub fn is_enabled(&self, kind: WorkerKind) -> bool {
        match kind {
            WorkerKind::Claude => self.claude.enabled,
            WorkerKind::Codex => self.codex.enabled,
            WorkerKind::Pi => self.pi.enabled,
            WorkerKind::Opencode => self.opencode.enabled,
            WorkerKind::Fake => self.fake.enabled,
        }
    }
}

// `Default` can't be derived because `kill_grace` is not zero.
#[allow(clippy::derivable_impls)]
impl Default for Workers {
    fn default() -> Self {
        Self {
            kill_grace: secs(10),
            claude: ClaudeWorker::default(),
            codex: CodexWorker::default(),
            pi: PiWorker::default(),
            opencode: OpencodeWorker::default(),
            fake: FakeWorker::default(),
        }
    }
}

/// `claude --permission-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClaudePermissionMode {
    /// Read-only planning mode.
    #[serde(rename = "plan")]
    Plan,
    /// Auto-accept file edits inside the workspace.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Claude's default interactive mode.
    #[serde(rename = "default")]
    Default,
    /// Bypass all permission prompts — only with `sandbox.tier = "container"`.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

/// How a `TaskSpec.output_schema` is requested from the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    /// Pass the schema with `--json-schema`.
    JsonSchema,
    /// Never pass a schema; rely on extraction from the final text.
    None,
}

/// `[workers.claude]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClaudeWorker {
    /// Adapter enabled.
    pub enabled: bool,
    /// Binary name or path.
    pub bin: String,
    /// `plan | acceptEdits | default`; `bypassPermissions` only in container tier.
    pub permission_mode: ClaudePermissionMode,
    /// `--allowedTools` allow-list.
    pub allowed_tools: Vec<String>,
    /// Extra argv appended to every invocation.
    pub extra_args: Vec<String>,
    /// Environment variables passed through to the subprocess.
    pub env_passthrough: Vec<String>,
    /// `--max-turns`.
    pub max_turns: u32,
    /// How structured output is requested.
    pub structured_output: StructuredOutput,
}

impl Default for ClaudeWorker {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "claude".into(),
            permission_mode: ClaudePermissionMode::AcceptEdits,
            allowed_tools: strings(&[
                "Read",
                "Edit",
                "Write",
                "Bash(git *)",
                "Bash(cargo *)",
                "Bash(npm *)",
                "Bash(pnpm *)",
                "Bash(bun *)",
                "Grep",
                "Glob",
            ]),
            extra_args: Vec::new(),
            env_passthrough: strings(&[
                "ANTHROPIC_API_KEY",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "HOME",
                "PATH",
                "SSL_CERT_FILE",
            ]),
            max_turns: 200,
            structured_output: StructuredOutput::JsonSchema,
        }
    }
}

/// `codex -s <sandbox>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandbox {
    /// No writes.
    ReadOnly,
    /// Writes inside the workspace only.
    WorkspaceWrite,
    /// No sandbox — only with `sandbox.tier = "container"`.
    DangerFullAccess,
}

/// `[workers.codex]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CodexWorker {
    /// Adapter enabled.
    pub enabled: bool,
    /// Binary name or path.
    pub bin: String,
    /// Codex sandbox mode.
    pub sandbox: CodexSandbox,
    /// Extra argv appended to every invocation.
    pub extra_args: Vec<String>,
    /// Environment variables passed through to the subprocess.
    pub env_passthrough: Vec<String>,
}

impl Default for CodexWorker {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "codex".into(),
            sandbox: CodexSandbox::WorkspaceWrite,
            extra_args: strings(&["--skip-git-repo-check"]),
            env_passthrough: strings(&["OPENAI_API_KEY", "CODEX_HOME", "HOME", "PATH"]),
        }
    }
}

/// `[workers.pi]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PiWorker {
    /// Adapter enabled.
    pub enabled: bool,
    /// Binary name or path.
    pub bin: String,
    /// Extra argv appended to every invocation.
    pub extra_args: Vec<String>,
    /// Environment variables passed through to the subprocess.
    pub env_passthrough: Vec<String>,
}

impl Default for PiWorker {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "pi".into(),
            extra_args: strings(&["--no-session"]),
            env_passthrough: strings(&[
                "HOME",
                "PATH",
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
            ]),
        }
    }
}

/// `[workers.opencode]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpencodeWorker {
    /// Adapter enabled.
    pub enabled: bool,
    /// Binary name or path.
    pub bin: String,
    /// Optional `--agent <name>`; empty = default.
    pub agent: String,
    /// Extra argv appended to every invocation.
    pub extra_args: Vec<String>,
    /// Environment variables passed through to the subprocess.
    pub env_passthrough: Vec<String>,
}

impl Default for OpencodeWorker {
    fn default() -> Self {
        Self {
            enabled: true,
            bin: "opencode".into(),
            agent: String::new(),
            extra_args: Vec::new(),
            env_passthrough: strings(&["HOME", "PATH", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"]),
        }
    }
}

/// `[workers.fake]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FakeWorker {
    /// Tests & Kohral conformance set true.
    pub enabled: bool,
    /// Path to a YAML/JSON scenario (`plan/04-workers.md`).
    pub script: PathBuf,
}

// ---------------------------------------------------------------------------
// [models.<alias>]
// ---------------------------------------------------------------------------

/// Price/capability tier of a model alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Cheap and quick.
    Fast,
    /// Default trade-off.
    Balanced,
    /// Most capable.
    Frontier,
}

impl Tier {
    /// Lowercase name, identical to the config form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Balanced => "balanced",
            Tier::Frontier => "frontier",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[models.<alias>]` entry. Unknown keys are collected in `extra` and
/// validated by the owning worker adapter (`Worker::validate_alias`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Which adapter runs this alias.
    pub worker: WorkerKind,
    /// Provider model id as the worker understands it.
    pub model: String,
    /// Capability tier.
    #[serde(default = "ModelEntry::default_tier")]
    pub tier: Tier,
    /// Context window, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// USD per 1M input tokens; unknown → cost accounting reports null.
    #[serde(
        default,
        with = "rust_decimal::serde::float_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_usd_per_m: Option<Decimal>,
    /// USD per 1M output tokens.
    #[serde(
        default,
        with = "rust_decimal::serde::float_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_usd_per_m: Option<Decimal>,
    /// Free-form capability tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Worker-specific keys (e.g. pi's `provider`).
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl ModelEntry {
    fn default_tier() -> Tier {
        Tier::Balanced
    }

    /// Minimal entry: `worker`, `model`, balanced tier, no prices, no tags.
    #[must_use]
    pub fn new(worker: WorkerKind, model: impl Into<String>) -> Self {
        Self {
            worker,
            model: model.into(),
            tier: Tier::Balanced,
            context_tokens: None,
            input_usd_per_m: None,
            output_usd_per_m: None,
            tags: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    /// Builder: tier.
    #[must_use]
    pub fn tier(mut self, tier: Tier) -> Self {
        self.tier = tier;
        self
    }

    /// Builder: context window.
    #[must_use]
    pub fn context_tokens(mut self, n: u64) -> Self {
        self.context_tokens = Some(n);
        self
    }

    /// Builder: prices in USD cents per 1M tokens (`500` = `5.00`).
    #[must_use]
    pub fn price_cents(mut self, input: i64, output: i64) -> Self {
        self.input_usd_per_m = Some(usd_cents(input));
        self.output_usd_per_m = Some(usd_cents(output));
        self
    }

    /// Builder: tags.
    #[must_use]
    pub fn tags(mut self, tags: &[&str]) -> Self {
        self.tags = strings(tags);
        self
    }

    /// Builder: one worker-specific extra key.
    #[must_use]
    pub fn extra(mut self, key: &str, value: impl Into<toml::Value>) -> Self {
        self.extra.insert(key.to_owned(), value.into());
        self
    }

    /// The `provider` extra key (required by `pi` aliases), if present and a string.
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.extra.get("provider").and_then(toml::Value::as_str)
    }
}

/// The built-in model catalog (`plan/03` §model catalog).
#[must_use]
pub fn default_models() -> BTreeMap<ModelAlias, ModelEntry> {
    [
        (
            "opus5-claude",
            ModelEntry::new(WorkerKind::Claude, "claude-opus-5")
                .tier(Tier::Frontier)
                .context_tokens(1_000_000)
                .price_cents(500, 2500)
                .tags(&["reasoning", "coding", "planning", "judge"]),
        ),
        (
            "fable5-claude",
            ModelEntry::new(WorkerKind::Claude, "claude-fable-5")
                .tier(Tier::Frontier)
                .context_tokens(1_000_000)
                .price_cents(1000, 5000)
                .tags(&["reasoning", "planning", "judge", "hard"]),
        ),
        (
            "sonnet5-claude",
            ModelEntry::new(WorkerKind::Claude, "claude-sonnet-5")
                .tier(Tier::Balanced)
                .context_tokens(1_000_000)
                .price_cents(300, 1500)
                .tags(&["coding", "implement", "test", "review"]),
        ),
        (
            "haiku45-claude",
            ModelEntry::new(WorkerKind::Claude, "claude-haiku-4-5")
                .tier(Tier::Fast)
                .context_tokens(200_000)
                .price_cents(100, 500)
                .tags(&["summarise", "classify", "cheap"]),
        ),
        (
            "gpt56-codex",
            ModelEntry::new(WorkerKind::Codex, "gpt-5.6")
                .tier(Tier::Frontier)
                .tags(&["coding", "implement", "review"]),
        ),
        (
            "sonnet5-pi",
            ModelEntry::new(WorkerKind::Pi, "claude-sonnet-5")
                .tier(Tier::Balanced)
                .price_cents(300, 1500)
                .tags(&["coding"])
                .extra("provider", "anthropic"),
        ),
        (
            "sonnet5-opencode",
            ModelEntry::new(WorkerKind::Opencode, "anthropic/claude-sonnet-5")
                .tier(Tier::Balanced)
                .price_cents(300, 1500)
                .tags(&["coding"]),
        ),
        (
            "fake",
            ModelEntry::new(WorkerKind::Fake, "fake")
                .tier(Tier::Fast)
                .price_cents(0, 0),
        ),
    ]
    .into_iter()
    .map(|(name, entry)| (alias(name), entry))
    .collect()
}

// ---------------------------------------------------------------------------
// [roles]
// ---------------------------------------------------------------------------

/// Orchestration roles bound to a model alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Understanding + plan.
    Planner,
    /// Question drafting.
    Clarifier,
    /// Evaluation.
    Judge,
    /// Merge/integration step.
    Integrator,
}

impl Role {
    /// Every role.
    pub const ALL: [Role; 4] = [
        Role::Planner,
        Role::Clarifier,
        Role::Judge,
        Role::Integrator,
    ];

    /// Lowercase name, identical to the config form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Clarifier => "clarifier",
            Role::Judge => "judge",
            Role::Integrator => "integrator",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[roles]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    /// Understanding + plan.
    pub planner: ModelAlias,
    /// Question drafting.
    pub clarifier: ModelAlias,
    /// Evaluation.
    pub judge: ModelAlias,
    /// Merge/integration step.
    pub integrator: ModelAlias,
    /// Fallback when routing has no candidates.
    pub default: ModelAlias,
    /// Reasoning effort per role.
    pub effort: BTreeMap<Role, Effort>,
}

impl Roles {
    /// The alias bound to `role`.
    #[must_use]
    pub fn alias_for(&self, role: Role) -> &ModelAlias {
        match role {
            Role::Planner => &self.planner,
            Role::Clarifier => &self.clarifier,
            Role::Judge => &self.judge,
            Role::Integrator => &self.integrator,
        }
    }

    /// `(key path, alias)` for every role binding, in config order.
    #[must_use]
    pub fn bindings(&self) -> Vec<(&'static str, &ModelAlias)> {
        vec![
            ("roles.planner", &self.planner),
            ("roles.clarifier", &self.clarifier),
            ("roles.judge", &self.judge),
            ("roles.integrator", &self.integrator),
            ("roles.default", &self.default),
        ]
    }
}

impl Default for Roles {
    fn default() -> Self {
        Self {
            planner: alias("opus5-claude"),
            clarifier: alias("opus5-claude"),
            judge: alias("opus5-claude"),
            integrator: alias("sonnet5-claude"),
            default: alias("sonnet5-claude"),
            effort: [
                (Role::Planner, Effort::XHigh),
                (Role::Judge, Effort::High),
                (Role::Integrator, Effort::Medium),
            ]
            .into_iter()
            .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// [routing]
// ---------------------------------------------------------------------------

/// Route selection policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPolicy {
    /// Thompson sampling over route scores.
    Thompson,
    /// Epsilon-greedy.
    EpsilonGreedy,
    /// Always the first candidate.
    Fixed,
}

/// Preferred tier per task complexity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PreferTier {
    /// Low complexity.
    pub low: Tier,
    /// Medium complexity.
    pub medium: Tier,
    /// High complexity.
    pub high: Tier,
}

impl Default for PreferTier {
    fn default() -> Self {
        Self {
            low: Tier::Fast,
            medium: Tier::Balanced,
            high: Tier::Frontier,
        }
    }
}

/// `[routing.kinds.<kind>]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoutingKind {
    /// Candidate aliases, in preference order.
    pub candidates: Vec<ModelAlias>,
}

/// `[routing]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Routing {
    /// Selection policy.
    pub policy: RoutingPolicy,
    /// Epsilon for epsilon-greedy; exploration floor for Thompson.
    pub exploration: f64,
    /// Samples before a route may be exploited.
    pub min_samples_before_exploit: u32,
    /// Weight of quality in the combined score.
    pub quality_weight: f64,
    /// Weight of (1 - normalised cost).
    pub cost_weight: f64,
    /// Weight of (1 - normalised latency).
    pub latency_weight: f64,
    /// Preferred tier per complexity.
    pub prefer_tier_for_complexity: PreferTier,
    /// Candidates per task kind.
    pub kinds: BTreeMap<TaskKind, RoutingKind>,
}

impl Default for Routing {
    fn default() -> Self {
        let kinds = [
            (
                TaskKind::Implement,
                &["sonnet5-claude", "gpt56-codex", "opus5-claude"][..],
            ),
            (TaskKind::Test, &["sonnet5-claude", "gpt56-codex"][..]),
            (TaskKind::Review, &["opus5-claude", "gpt56-codex"][..]),
            (TaskKind::Research, &["opus5-claude", "sonnet5-claude"][..]),
            (TaskKind::Write, &["sonnet5-claude", "haiku45-claude"][..]),
            (
                TaskKind::Debug,
                &["opus5-claude", "gpt56-codex", "sonnet5-claude"][..],
            ),
            (TaskKind::Refactor, &["sonnet5-claude", "gpt56-codex"][..]),
            (TaskKind::Ops, &["sonnet5-claude"][..]),
        ]
        .into_iter()
        .map(|(kind, candidates)| {
            (
                kind,
                RoutingKind {
                    candidates: candidates.iter().map(|a| alias(a)).collect(),
                },
            )
        })
        .collect();
        Self {
            policy: RoutingPolicy::Thompson,
            exploration: 0.10,
            min_samples_before_exploit: 3,
            quality_weight: 0.7,
            cost_weight: 0.2,
            latency_weight: 0.1,
            prefer_tier_for_complexity: PreferTier::default(),
            kinds,
        }
    }
}

// ---------------------------------------------------------------------------
// [memory]
// ---------------------------------------------------------------------------

/// Embedding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Embedder {
    /// Local ONNX models via fastembed.
    Fastembed,
    /// No embeddings (memory retrieval disabled).
    None,
}

/// `[memory]` section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Memory {
    /// Memory enabled.
    pub enabled: bool,
    /// Embedding backend.
    pub embedder: Embedder,
    /// Embedding model name (changing it requires `kevin memory reindex`).
    pub embedding_model: String,
    /// Embedding dimensions; must match the model.
    pub dimensions: u32,
    /// Items retrieved per query.
    pub top_k: u32,
    /// Cosine similarity floor.
    pub min_similarity: f64,
    /// Cap of the rendered memory block injected into context.
    pub context_max_tokens: u32,
    /// Store run summaries.
    pub store_run_summaries: bool,
    /// Store artifact summaries.
    pub store_artifact_summaries: bool,
    /// Importance decay for ranking, never deletion.
    pub decay_half_life_days: u32,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            enabled: true,
            embedder: Embedder::Fastembed,
            embedding_model: "BAAI/bge-small-en-v1.5".into(),
            dimensions: 384,
            top_k: 8,
            min_similarity: 0.35,
            context_max_tokens: 2500,
            store_run_summaries: true,
            store_artifact_summaries: true,
            decay_half_life_days: 90,
        }
    }
}

/// Known embedding models and their dimensions (`memory.dimensions` must match).
pub const KNOWN_EMBEDDING_DIMENSIONS: &[(&str, u32)] = &[
    ("BAAI/bge-small-en-v1.5", 384),
    ("BAAI/bge-base-en-v1.5", 768),
    ("BAAI/bge-large-en-v1.5", 1024),
    ("sentence-transformers/all-MiniLM-L6-v2", 384),
    ("nomic-ai/nomic-embed-text-v1.5", 768),
];

// ---------------------------------------------------------------------------
// [evaluation]
// ---------------------------------------------------------------------------

/// What evaluations may change without a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoApply {
    /// Routing scores.
    Routing,
    /// Memory items / lessons.
    Memory,
}

/// `[evaluation]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Evaluation {
    /// Evaluation enabled.
    pub enabled: bool,
    /// Per-task judge pass (costs money); `false` evaluates only the run.
    pub evaluate_tasks: bool,
    /// Built-in rubric name or path to a TOML rubric.
    pub rubric: String,
    /// What evaluations may change without a human.
    pub auto_apply: Vec<AutoApply>,
    /// Prompt/config proposals are always just proposals.
    pub proposals_require_approval: bool,
}

impl Default for Evaluation {
    fn default() -> Self {
        Self {
            enabled: true,
            evaluate_tasks: true,
            rubric: "default".into(),
            auto_apply: vec![AutoApply::Routing, AutoApply::Memory],
            proposals_require_approval: true,
        }
    }
}

// ---------------------------------------------------------------------------
// [workspace]
// ---------------------------------------------------------------------------

/// Workspace isolation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStrategy {
    /// jj if `.jj` exists, else git worktree, else in place.
    Auto,
    /// `git worktree add`.
    GitWorktree,
    /// `jj workspace add`.
    JjWorkspace,
    /// Run in the target repo itself (read-only kinds only).
    InPlace,
}

/// When task workspaces are removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceCleanup {
    /// Remove after a successful attempt.
    OnSuccess,
    /// Always remove.
    Always,
    /// Never remove.
    Never,
}

/// How results are integrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Integration {
    /// Open a pull request.
    Pr,
    /// Merge locally.
    Merge,
    /// Leave branches.
    None,
}

/// `[workspace]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkspaceCfg {
    /// Isolation strategy.
    pub strategy: WorkspaceStrategy,
    /// Relative to the target repo.
    pub root: PathBuf,
    /// Branch prefix for task branches.
    pub branch_prefix: String,
    /// When to clean up.
    pub cleanup: WorkspaceCleanup,
    /// Integration mode.
    pub integration: Integration,
    /// `pr` mode: one PR per succeeded task instead of one integrated PR.
    pub pr_per_task: bool,
}

impl Default for WorkspaceCfg {
    fn default() -> Self {
        Self {
            strategy: WorkspaceStrategy::Auto,
            root: PathBuf::from(".kevin/workspaces"),
            branch_prefix: "kevin/".into(),
            cleanup: WorkspaceCleanup::OnSuccess,
            integration: Integration::Pr,
            pr_per_task: false,
        }
    }
}

/// `[checks]` section.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Checks {
    /// Repo checks run by the integrator before opening a PR (allowed in the project layer).
    pub commands: Vec<String>,
}

// ---------------------------------------------------------------------------
// [sandbox]
// ---------------------------------------------------------------------------

/// Sandbox tier (`plan/09-security.md` §Sandbox tiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxTier {
    /// Workers' own sandboxes + workspace scoping.
    CliNative,
    /// Kevin itself is containerised; bypass flags allowed.
    Container,
    /// Explicit opt-out.
    None,
}

impl SandboxTier {
    /// Config form (`cli-native`, `container`, `none`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxTier::CliNative => "cli-native",
            SandboxTier::Container => "container",
            SandboxTier::None => "none",
        }
    }
}

impl fmt::Display for SandboxTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Network policy for workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetwork {
    /// Inherit the host network.
    Inherit,
    /// Deny network (container tier only).
    Deny,
}

/// `[sandbox]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sandbox {
    /// Sandbox tier.
    pub tier: SandboxTier,
    /// Derived: true only when `tier = "container"`.
    pub allow_dangerous_flags: bool,
    /// Network policy.
    pub network: SandboxNetwork,
    /// Extra environment variables allowed through to every worker.
    pub env_allowlist_extra: Vec<String>,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self {
            tier: SandboxTier::CliNative,
            allow_dangerous_flags: false,
            network: SandboxNetwork::Inherit,
            env_allowlist_extra: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// [telemetry]
// ---------------------------------------------------------------------------

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// One JSON object per line.
    Json,
    /// Human-readable.
    Pretty,
}

/// `[telemetry]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Telemetry {
    /// Log format.
    pub log_format: LogFormat,
    /// `tracing` filter directive.
    pub log_level: String,
    /// Prometheus exporter bind; empty disables it.
    pub metrics_bind: String,
    /// OTLP endpoint; empty disables tracing export.
    pub otlp_endpoint: String,
}

impl Default for Telemetry {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Json,
            log_level: "info".into(),
            metrics_bind: String::new(),
            otlp_endpoint: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// [kohral]
// ---------------------------------------------------------------------------

/// `[kohral]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Kohral {
    /// `kevin serve --kohral` or `profile = "kohral"`.
    pub enabled: bool,
    /// Kohral-facing listener.
    pub bind: SocketAddr,
    /// Runtime token mounted by Kohral.
    pub token_file: PathBuf,
    /// Signed agent identity mounted by Kohral.
    pub identity_file: PathBuf,
    /// `KOHRAL_COLLABORATION_URL`.
    pub collaboration_url: String,
    /// Agent soul document.
    pub soul_file: PathBuf,
    /// Kohral documentation document.
    pub documentation_file: PathBuf,
    /// Agent memory document.
    pub memory_file: PathBuf,
    /// Per-turn run timeout.
    #[serde(with = "duration")]
    pub run_timeout: Duration,
    /// Per temporary attachment.
    pub max_attachment_bytes: u64,
}

impl Default for Kohral {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080),
            token_file: PathBuf::from("/run/secrets/kohral-runtime-token"),
            identity_file: PathBuf::from("/run/secrets/kohral-agent-identity"),
            collaboration_url: String::new(),
            soul_file: PathBuf::from("/opt/kevin/config/SOUL.md"),
            documentation_file: PathBuf::from("/opt/kevin/config/KOHRAL_DOCUMENTATION.md"),
            memory_file: PathBuf::from("/opt/kevin/data/MEMORY.md"),
            run_timeout: secs(30 * 60),
            max_attachment_bytes: 26_214_400,
        }
    }
}
