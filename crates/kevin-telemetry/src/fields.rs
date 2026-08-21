//! Span and field conventions (`plan/10-observability-ops.md` §Logging).
//!
//! Use these constants instead of string literals so that field names stay
//! stable across crates:
//!
//! ```
//! use kevin_telemetry::fields;
//! let run_id = "01910000-0000-7000-8000-0000000000aa";
//! let span = tracing::info_span!(fields::span::RUN, { fields::RUN_ID } = run_id);
//! ```

/// `run_id` — the run aggregate id.
pub const RUN_ID: &str = "run_id";
/// `task_id` — the task aggregate id.
pub const TASK_ID: &str = "task_id";
/// `attempt_id` — one worker attempt on a task.
pub const ATTEMPT_ID: &str = "attempt_id";
/// `question_id`.
pub const QUESTION_ID: &str = "question_id";
/// `worker` — worker kind (`claude`, `codex`, `pi`, `opencode`, `fake`).
pub const WORKER: &str = "worker";
/// `model_alias` — config-level model alias.
pub const MODEL_ALIAS: &str = "model_alias";
/// `task_kind` — task kind from the taxonomy.
pub const TASK_KIND: &str = "task_kind";
/// `command_id`.
pub const COMMAND_ID: &str = "command_id";
/// `correlation_id` — always the run id when one exists.
pub const CORRELATION_ID: &str = "correlation_id";
/// `causation_id` — command or event id that caused the current work.
pub const CAUSATION_ID: &str = "causation_id";
/// `kohral_turn_id`.
pub const KOHRAL_TURN_ID: &str = "kohral_turn_id";
/// `session_key` (Kohral).
pub const SESSION_KEY: &str = "session_key";
/// `event` — the stable machine event name (see [`crate::events`]).
pub const EVENT: &str = "event";
/// `error.class` — classified error kind, logged once at the owning boundary.
pub const ERROR_CLASS: &str = "error.class";
/// `error.message` — redacted error message.
pub const ERROR_MESSAGE: &str = "error.message";
/// `phase` — run phase name (`phase{name}` span).
pub const PHASE: &str = "phase";
/// `projection` — projection name.
pub const PROJECTION: &str = "projection";
/// `route` — HTTP route template (never a raw path).
pub const ROUTE: &str = "route";
/// `method` — HTTP method.
pub const METHOD: &str = "method";

/// All span field names carried by the JSON record, in output order.
pub const ALL: &[&str] = &[
    RUN_ID,
    TASK_ID,
    ATTEMPT_ID,
    QUESTION_ID,
    WORKER,
    MODEL_ALIAS,
    TASK_KIND,
    COMMAND_ID,
    CORRELATION_ID,
    CAUSATION_ID,
    KOHRAL_TURN_ID,
    SESSION_KEY,
];

/// Span names: `run` → `phase` → `task` → `attempt` → `worker_process`;
/// `command`, `projection`, `http`.
pub mod span {
    /// `run` span (fields: `run_id`, `correlation_id`).
    pub const RUN: &str = "run";
    /// `phase` span (field: `phase` name).
    pub const PHASE: &str = "phase";
    /// `task` span (fields: `task_id`, `task_kind`).
    pub const TASK: &str = "task";
    /// `attempt` span (fields: `attempt_id`, `worker`, `model_alias`).
    pub const ATTEMPT: &str = "attempt";
    /// `worker_process` span.
    pub const WORKER_PROCESS: &str = "worker_process";
    /// `command` span (field: `command_id`, `type`).
    pub const COMMAND: &str = "command";
    /// `projection` span (field: `projection`).
    pub const PROJECTION: &str = "projection";
    /// `http` span (fields: `method`, `route`).
    pub const HTTP: &str = "http";
}
