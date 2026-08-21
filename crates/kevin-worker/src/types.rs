//! Request-side value objects of the workers context (`plan/04-workers.md`
//! §Core types, `plan/02-domain-model.md` §Identifiers and value objects).
//!
//! Several of these mirror domain value objects that WS-01 is defining in
//! `kevin-domain` concurrently (`TaskSpec`, `Route`, `Usage`, `Workspace`,
//! `ArtifactRef`) and the config `ModelEntry` from WS-02. They carry the exact
//! field names of the plan so the later merge is a type-alias swap; each one is
//! marked with a `TODO(ws-NN)`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Add, AddAssign};
use std::path::PathBuf;
use std::time::Duration;

use kevin_domain::{AttemptId, Effort, ModelAlias, RunId, TaskId, TaskKind, WorkerKind};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use crate::worker::WorkerSessionId;

// ---------------------------------------------------------------------------
// Domain value objects (TODO(ws-01): switch to kevin_domain::…)
// ---------------------------------------------------------------------------

/// Workspace policy requested by the plan for a task.
// TODO(ws-01): switch to `kevin_domain::WorkspacePolicy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePolicy {
    /// The attempt gets its own isolated workspace (default).
    #[default]
    Isolated,
    /// In-place, read-only execution (`research`, `write`, `review` kinds only).
    ReadOnly,
}

/// What a task asks a worker to do.
// TODO(ws-01): switch to `kevin_domain::TaskSpec` (same field names).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Short title.
    pub title: String,
    /// Full instructions (the prompt body).
    pub instructions: String,
    /// Input artifacts.
    #[serde(default)]
    pub inputs: Vec<ArtifactRef>,
    /// Acceptance criteria from the approved plan.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Tasks this one depends on.
    #[serde(default)]
    pub depends_on: Vec<TaskId>,
    /// Workspace policy.
    #[serde(default)]
    pub workspace_policy: WorkspacePolicy,
    /// JSON schema the final answer must match, if structured output is wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

impl TaskSpec {
    /// A spec with only a title and instructions.
    pub fn new(title: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            instructions: instructions.into(),
            ..Self::default()
        }
    }

    /// Sets the output schema.
    #[must_use]
    pub fn with_output_schema(mut self, schema: serde_json::Value) -> Self {
        self.output_schema = Some(schema);
        self
    }
}

/// The `(worker, model alias, effort)` chosen for an attempt.
// TODO(ws-01): switch to `kevin_domain::Route`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Route {
    /// Which worker runs the attempt.
    pub worker: WorkerKind,
    /// Config alias of the model.
    pub model: ModelAlias,
    /// Requested effort, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

/// Token and cost accounting; additive.
// TODO(ws-01): switch to `kevin_domain::Usage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Completion tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Tokens served from a prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Tokens written to a prompt cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Cost in USD when the worker reports it; `None` → router price table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<Decimal>,
    /// Wall-clock milliseconds.
    #[serde(default)]
    pub wall_ms: u64,
}

impl Usage {
    /// Usage with only input/output tokens.
    #[must_use]
    pub fn tokens(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            ..Self::default()
        }
    }

    /// `true` when every counter is zero and no cost is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Input + output tokens (the budget dimension `max_tokens` counts).
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

impl Add for Usage {
    type Output = Usage;

    fn add(mut self, rhs: Usage) -> Usage {
        self += rhs;
        self
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(rhs.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(rhs.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(rhs.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(rhs.cache_write_tokens);
        self.wall_ms = self.wall_ms.saturating_add(rhs.wall_ms);
        self.cost_usd = match (self.cost_usd, rhs.cost_usd) {
            (None, None) => None,
            (Some(a), None) | (None, Some(a)) => Some(a),
            (Some(a), Some(b)) => Some(a + b),
        };
    }
}

/// How the attempt's working directory was produced.
// TODO(ws-01): switch to `kevin_domain::WorkspaceKind`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// The repository itself.
    InPlace,
    /// A git worktree on `branch`.
    GitWorktree {
        /// Branch checked out in the worktree.
        branch: String,
    },
    /// A jj workspace named `name`.
    JjWorkspace {
        /// Workspace name.
        name: String,
    },
}

/// The isolated checkout an attempt runs in (`cwd` of the worker process).
// TODO(ws-01): switch to `kevin_domain::Workspace`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Workspace {
    /// Working directory of the worker process.
    pub root: PathBuf,
    /// How it was produced.
    #[serde(flatten)]
    pub kind: WorkspaceKind,
    /// Revision the workspace was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_rev: Option<String>,
}

impl Workspace {
    /// An in-place workspace rooted at `root`.
    pub fn in_place(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            kind: WorkspaceKind::InPlace,
            base_rev: None,
        }
    }
}

/// Kind of an artifact.
// TODO(ws-01): switch to `kevin_domain::ArtifactKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A unified diff.
    Diff,
    /// A plain file.
    File,
    /// A pull-request URL.
    PrUrl,
    /// A textual report.
    Report,
    /// Structured JSON.
    Json,
    /// Raw worker transcript (JSONL).
    Transcript,
}

/// Reference to an artifact stored under `data_dir` (or remotely).
// TODO(ws-01): switch to `kevin_domain::ArtifactRef`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Artifact id.
    pub id: Uuid,
    /// What the artifact is.
    pub kind: ArtifactKind,
    /// Where it lives (`file://…`, `https://…`).
    pub uri: String,
    /// Hex sha256 of the content.
    pub sha256: String,
    /// Size in bytes.
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Config value objects (from kevin-config, WS-02)
// ---------------------------------------------------------------------------

pub use kevin_config::{ConfigError, ConfigErrors, ModelEntry, Tier};

// ---------------------------------------------------------------------------
// Worker-side request types (owned here)
// ---------------------------------------------------------------------------

/// Extra context handed to the worker besides the task spec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptContext {
    /// Kevin briefing appended to the worker's system prompt (task title,
    /// acceptance criteria, lessons, operator instructions).
    #[serde(default)]
    pub system_prompt_append: String,
    /// Rendered `<kevin-memory>` block (`plan/06` §1.6), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Worker-native session to resume for follow-up attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_session: Option<WorkerSessionId>,
}

/// Names of the environment variables a worker process may inherit
/// (`workers.<kind>.env_passthrough` + `sandbox.env_allowlist_extra`).
///
/// Only *names* are kept; values are read from Kevin's own environment at
/// spawn time by [`EnvAllowlist::resolve`]. Nothing outside the list is ever
/// inherited (`plan/09-security.md` §Environment and secrets).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvAllowlist {
    names: BTreeSet<String>,
}

impl EnvAllowlist {
    /// An allow-list of the given names.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// `workers.<kind>.env_passthrough` ∪ `sandbox.env_allowlist_extra`.
    #[must_use]
    pub fn build(passthrough: &[String], extra: &[String]) -> Self {
        Self::new(passthrough.iter().chain(extra).cloned())
    }

    /// The allowed names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Whether `name` may be inherited.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    /// Number of allowed names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// `true` when nothing is allowed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// Reads the allowed variables from the current process environment.
    /// Unset variables and non-UTF-8 values are skipped.
    #[must_use]
    pub fn resolve(&self) -> BTreeMap<String, String> {
        self.names
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect()
    }
}

/// Limits of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptBudget {
    /// Wall-clock timeout (`budget.default_task_wall` unless the spec overrides).
    pub timeout: Duration,
    /// Soft token cap (input + output).
    pub max_tokens: Option<u64>,
    /// Hard turn cap where the worker supports it (`claude --max-turns`).
    pub max_turns: Option<u32>,
}

impl Default for AttemptBudget {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30 * 60),
            max_tokens: None,
            max_turns: None,
        }
    }
}

impl AttemptBudget {
    /// A budget with only a timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }
}

/// Everything a worker needs to run one attempt (`plan/04-workers.md` §Core types).
#[derive(Debug, Clone)]
pub struct TaskAttemptRequest {
    /// Attempt id (also the worker session id for fresh attempts).
    pub attempt_id: AttemptId,
    /// Task being attempted.
    pub task_id: TaskId,
    /// Correlation id for logs/transcripts.
    pub run_id: RunId,
    /// Task kind.
    pub kind: TaskKind,
    /// Title, instructions, inputs, acceptance criteria, output schema.
    pub spec: TaskSpec,
    /// Worker, model alias, effort.
    pub route: Route,
    /// Resolved `[models.<alias>]` entry.
    pub model: ModelEntry,
    /// `cwd` for the process.
    pub workspace: Workspace,
    /// System prompt append, memory block, prior session.
    pub context: AttemptContext,
    /// Allow-listed environment variable names.
    pub env: EnvAllowlist,
    /// Timeout and caps.
    pub budget: AttemptBudget,
    /// Child of the task token; cancelling it stops the attempt.
    pub cancel: CancellationToken,
}

impl TaskAttemptRequest {
    /// The text a prompt-matching worker (the fake) sees: title and instructions.
    #[must_use]
    pub fn prompt_text(&self) -> String {
        if self.spec.title.is_empty() {
            self.spec.instructions.clone()
        } else {
            format!("{}\n{}", self.spec.title, self.spec.instructions)
        }
    }

    /// The variables Kevin always sets for a worker process
    /// (`plan/09-security.md`): `KEVIN_RUN_ID`, `KEVIN_TASK_ID`,
    /// `KEVIN_ATTEMPT_ID`, `KEVIN_WORKSPACE`.
    #[must_use]
    pub fn kevin_env(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("KEVIN_RUN_ID".to_owned(), self.run_id.to_string()),
            ("KEVIN_TASK_ID".to_owned(), self.task_id.to_string()),
            ("KEVIN_ATTEMPT_ID".to_owned(), self.attempt_id.to_string()),
            (
                "KEVIN_WORKSPACE".to_owned(),
                self.workspace.root.to_string_lossy().into_owned(),
            ),
        ])
    }

    /// Allow-listed environment plus the Kevin variables — what the process gets.
    #[must_use]
    pub fn process_env(&self) -> BTreeMap<String, String> {
        let mut env = self.env.resolve();
        env.extend(self.kevin_env());
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_additive_and_cost_merges() {
        let a = Usage {
            cost_usd: Some(Decimal::new(5, 2)),
            ..Usage::tokens(10, 5)
        };
        let b = Usage {
            cache_read_tokens: 3,
            wall_ms: 100,
            ..Usage::tokens(1, 1)
        };
        let sum = a.clone() + b.clone();
        assert_eq!(sum.input_tokens, 11);
        assert_eq!(sum.output_tokens, 6);
        assert_eq!(sum.cache_read_tokens, 3);
        assert_eq!(sum.wall_ms, 100);
        assert_eq!(sum.cost_usd, Some(Decimal::new(5, 2)));
        assert_eq!(sum.total_tokens(), 17);
        let both = a + Usage {
            cost_usd: Some(Decimal::new(1, 2)),
            ..Usage::default()
        };
        assert_eq!(both.cost_usd, Some(Decimal::new(6, 2)));
        assert!(Usage::default().is_empty());
        assert!(!b.is_empty());
    }

    #[test]
    fn env_allowlist_keeps_names_only_and_resolves_from_process_env() {
        let list = EnvAllowlist::build(
            &["PATH".to_owned(), "HOME".to_owned()],
            &["KEVIN_TEST_SURELY_UNSET_VAR".to_owned()],
        );
        assert_eq!(list.len(), 3);
        assert!(list.contains("PATH"));
        assert!(!list.contains("CARGO_MANIFEST_DIR"));
        let resolved = list.resolve();
        assert!(resolved.contains_key("PATH"));
        assert!(!resolved.contains_key("KEVIN_TEST_SURELY_UNSET_VAR"));
        assert!(!resolved.contains_key("CARGO_MANIFEST_DIR"));
    }
}
