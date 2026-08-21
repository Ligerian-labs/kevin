//! Value objects shared by every aggregate (`plan/02-domain-model.md`
//! §Identifiers and value objects).
//!
//! All types here are plain data with serde derives; validation that needs
//! more than the type system lives in small `validate()` helpers that the
//! aggregates call from `handle`.

use std::fmt;
use std::ops::{Add, AddAssign};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::envelope::Actor;
use crate::ids::{ArtifactId, EvaluationId, ProposalId, RunId, TaskId};
use crate::kinds::{Effort, ModelAlias, WorkerKind};

/// A JSON schema (or any JSON document) carried by a task spec or plan task.
pub type JsonSchema = serde_json::Value;

// ---------------------------------------------------------------------------
// Durations on the wire
// ---------------------------------------------------------------------------

/// Serde adapters encoding [`Duration`] as whole milliseconds (`u64`), which is
/// what every Kevin JSON surface uses for durations.
pub mod duration_ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// Serialises a duration as milliseconds.
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        u64::try_from(d.as_millis())
            .unwrap_or(u64::MAX)
            .serialize(s)
    }

    /// Deserialises milliseconds into a duration.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }

    /// Same adapters for `Option<Duration>`.
    pub mod option {
        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serialize, Serializer};

        /// Serialises an optional duration as milliseconds or `null`.
        pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
            d.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                .serialize(s)
        }

        /// Deserialises milliseconds or `null`.
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
            Ok(Option::<u64>::deserialize(d)?.map(Duration::from_millis))
        }
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// The `(worker, model alias, effort)` chosen for a task attempt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Route {
    /// Which adapter runs the attempt.
    pub worker: WorkerKind,
    /// Config-level model alias.
    pub model: ModelAlias,
    /// Requested effort; `None` = the alias/role default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl Route {
    /// A route without an explicit effort.
    #[must_use]
    pub const fn new(worker: WorkerKind, model: ModelAlias) -> Self {
        Self {
            worker,
            model,
            effort: None,
        }
    }

    /// Sets the effort.
    #[must_use]
    pub const fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.worker, self.model)?;
        if let Some(effort) = self.effort {
            write!(f, "@{effort}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Budget / Usage
// ---------------------------------------------------------------------------

/// Limits attached to a run or a task.
///
/// Every limit is optional except `max_attempts` (default 2) and
/// `max_parallel` (default 4), mirroring `[budget]` in `plan/03-config-schema.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    /// Maximum spend in USD.
    pub max_usd: Option<Decimal>,
    /// Maximum tokens (input + output reported by workers).
    pub max_tokens: Option<u64>,
    /// Maximum wall-clock time (milliseconds on the wire).
    #[serde(with = "duration_ms::option")]
    pub max_wall: Option<Duration>,
    /// Maximum attempts per task.
    pub max_attempts: u8,
    /// Maximum concurrently running attempts.
    pub max_parallel: u16,
}

impl Budget {
    /// Default `max_attempts`.
    pub const DEFAULT_MAX_ATTEMPTS: u8 = 2;
    /// Default `max_parallel`.
    pub const DEFAULT_MAX_PARALLEL: u16 = 4;

    /// A budget with no limits except the attempt/parallel defaults.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_usd: None,
            max_tokens: None,
            max_wall: None,
            max_attempts: Self::DEFAULT_MAX_ATTEMPTS,
            max_parallel: Self::DEFAULT_MAX_PARALLEL,
        }
    }

    /// Sets `max_usd`.
    #[must_use]
    pub const fn with_max_usd(mut self, usd: Decimal) -> Self {
        self.max_usd = Some(usd);
        self
    }

    /// Sets `max_tokens`.
    #[must_use]
    pub const fn with_max_tokens(mut self, tokens: u64) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    /// Sets `max_wall`.
    #[must_use]
    pub const fn with_max_wall(mut self, wall: Duration) -> Self {
        self.max_wall = Some(wall);
        self
    }

    /// Sets `max_attempts`.
    #[must_use]
    pub const fn with_max_attempts(mut self, attempts: u8) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// Sets `max_parallel`.
    #[must_use]
    pub const fn with_max_parallel(mut self, parallel: u16) -> Self {
        self.max_parallel = parallel;
        self
    }

    /// The first dimension (usd, then tokens) that `usage` exceeds, if any.
    ///
    /// Wall-clock is not checked here: `Usage::wall_ms` sums attempt durations
    /// (which overlap), while the wall budget is about elapsed run time and is
    /// enforced by the orchestrator's tick (`plan/05-orchestration.md` §4).
    #[must_use]
    pub fn exceeded_by(&self, usage: &Usage) -> Option<BudgetExcess> {
        if let (Some(limit), Some(cost)) = (self.max_usd, usage.cost_usd)
            && cost > limit
        {
            return Some(BudgetExcess {
                dimension: BudgetDimension::Usd,
                limit,
                actual: cost,
            });
        }
        if let Some(limit) = self.max_tokens {
            let tokens = usage.total_tokens();
            if tokens > limit {
                return Some(BudgetExcess {
                    dimension: BudgetDimension::Tokens,
                    limit: Decimal::from(limit),
                    actual: Decimal::from(tokens),
                });
            }
        }
        None
    }

    /// Checks the wall-clock dimension against an elapsed duration.
    #[must_use]
    pub fn wall_exceeded_by(&self, elapsed: Duration) -> Option<BudgetExcess> {
        let limit = self.max_wall?;
        (elapsed > limit).then(|| BudgetExcess {
            dimension: BudgetDimension::Wall,
            limit: Decimal::from(u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)),
            actual: Decimal::from(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
        })
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Which budget limit was crossed (`run.budget_exhausted.dimension`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetDimension {
    /// `Budget::max_usd`.
    Usd,
    /// `Budget::max_tokens`.
    Tokens,
    /// `Budget::max_wall`.
    Wall,
}

impl BudgetDimension {
    /// Lowercase name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BudgetDimension::Usd => "usd",
            BudgetDimension::Tokens => "tokens",
            BudgetDimension::Wall => "wall",
        }
    }
}

impl fmt::Display for BudgetDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A crossed budget limit: which dimension, the limit and the observed value
/// (USD for `usd`, a count for `tokens`, milliseconds for `wall`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetExcess {
    /// Which limit.
    pub dimension: BudgetDimension,
    /// The configured limit.
    pub limit: Decimal,
    /// The observed value.
    pub actual: Decimal,
}

/// Resource usage reported by workers; additive.
///
/// `cost_usd` is `None` until a worker or the router's price table provides
/// it; adding `None` to `Some(x)` yields `Some(x)` (unknown cost never erases
/// known cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Completion tokens.
    pub output_tokens: u64,
    /// Tokens served from prompt cache.
    pub cache_read_tokens: u64,
    /// Tokens written to prompt cache.
    pub cache_write_tokens: u64,
    /// Cost in USD when known.
    pub cost_usd: Option<Decimal>,
    /// Wall-clock milliseconds spent.
    pub wall_ms: u64,
}

impl Usage {
    /// Zero usage.
    pub const ZERO: Usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        cost_usd: None,
        wall_ms: 0,
    };

    /// `input_tokens + output_tokens` (what `Budget::max_tokens` counts).
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// `true` when every counter is zero and the cost is unknown.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        *self == Self::ZERO
    }

    /// Sets `cost_usd`.
    #[must_use]
    pub const fn with_cost_usd(mut self, cost: Decimal) -> Self {
        self.cost_usd = Some(cost);
        self
    }
}

impl Add for Usage {
    type Output = Usage;

    fn add(self, rhs: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens.saturating_add(rhs.input_tokens),
            output_tokens: self.output_tokens.saturating_add(rhs.output_tokens),
            cache_read_tokens: self.cache_read_tokens.saturating_add(rhs.cache_read_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_add(rhs.cache_write_tokens),
            cost_usd: match (self.cost_usd, rhs.cost_usd) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
            wall_ms: self.wall_ms.saturating_add(rhs.wall_ms),
        }
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Usage) {
        *self = *self + rhs;
    }
}

impl std::iter::Sum for Usage {
    fn sum<I: Iterator<Item = Usage>>(iter: I) -> Usage {
        iter.fold(Usage::ZERO, Add::add)
    }
}

impl<'a> std::iter::Sum<&'a Usage> for Usage {
    fn sum<I: Iterator<Item = &'a Usage>>(iter: I) -> Usage {
        iter.fold(Usage::ZERO, |acc, u| acc + *u)
    }
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

/// How a task's workspace is isolated from the user's checkout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// Work directly in the run's `cwd`.
    InPlace,
    /// A `git worktree` on its own branch.
    GitWorktree {
        /// Branch checked out in the worktree.
        branch: String,
    },
    /// A `jj workspace`.
    JjWorkspace {
        /// Workspace name.
        name: String,
    },
}

/// An isolated checkout a worker runs in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Workspace {
    /// Absolute path of the checkout.
    pub root: PathBuf,
    /// Isolation strategy.
    pub kind: WorkspaceKind,
    /// Revision the workspace was created from (diff base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_rev: Option<String>,
}

impl Workspace {
    /// An in-place workspace at `root`.
    #[must_use]
    pub fn in_place(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            kind: WorkspaceKind::InPlace,
            base_rev: None,
        }
    }
}

/// Plan-level hint for how a task's workspace must be prepared
/// (`kevin.plan.v1` `workspace_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePolicy {
    /// Own worktree/workspace (default).
    #[default]
    Isolated,
    /// Shares the run's workspace; serialised with other writers.
    Shared,
    /// Read-only checkout (research, review).
    ReadOnly,
}

impl WorkspacePolicy {
    /// Every policy.
    pub const ALL: [WorkspacePolicy; 3] = [
        WorkspacePolicy::Isolated,
        WorkspacePolicy::Shared,
        WorkspacePolicy::ReadOnly,
    ];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            WorkspacePolicy::Isolated => "isolated",
            WorkspacePolicy::Shared => "shared",
            WorkspacePolicy::ReadOnly => "read_only",
        }
    }
}

impl fmt::Display for WorkspacePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Version-control flavour detected at the run's `cwd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoKind {
    /// `.git` present.
    Git,
    /// `.jj` present (takes precedence over git when colocated).
    Jj,
    /// No repository; workspaces fall back to in-place.
    #[default]
    None,
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// What an artifact is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A unified diff.
    Diff,
    /// A file path (under the data dir or the workspace).
    File,
    /// A pull-request URL.
    PrUrl,
    /// A human-readable report.
    Report,
    /// Structured JSON output.
    Json,
    /// A worker transcript.
    Transcript,
}

/// Reference to an artifact produced by a task; the bytes live in
/// `orch.artifacts` or on disk under the data dir.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Artifact id.
    pub id: ArtifactId,
    /// Kind.
    pub kind: ArtifactKind,
    /// Where the bytes are (`file://…`, `https://…`, `artifact://<id>`).
    pub uri: String,
    /// Hex SHA-256 of the bytes when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Size in bytes when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

// ---------------------------------------------------------------------------
// Evaluation values
// ---------------------------------------------------------------------------

/// One criterion score from a judge (0..=10).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RubricScore {
    /// Criterion key from the rubric (`correctness`, …).
    pub criterion: String,
    /// 0..=10.
    pub score: u8,
    /// Why.
    pub rationale: String,
}

impl RubricScore {
    /// Highest allowed score.
    pub const MAX_SCORE: u8 = 10;

    /// Builds a score after checking `score <= 10`.
    pub fn new(
        criterion: impl Into<String>,
        score: u8,
        rationale: impl Into<String>,
    ) -> Result<Self, InvalidValue> {
        let value = Self {
            criterion: criterion.into(),
            score,
            rationale: rationale.into(),
        };
        value.validate()?;
        Ok(value)
    }

    /// Checks `score <= 10` and a non-empty criterion.
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.criterion.trim().is_empty() {
            return Err(InvalidValue::new("criterion", "must not be empty"));
        }
        if self.score > Self::MAX_SCORE {
            return Err(InvalidValue::new(
                "score",
                format!("{} exceeds the maximum of {}", self.score, Self::MAX_SCORE),
            ));
        }
        Ok(())
    }
}

/// Judge verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Good as is.
    Accept,
    /// Acceptable with follow-up fixes.
    AcceptWithFixes,
    /// Not acceptable.
    Reject,
}

impl Verdict {
    /// Every verdict, best first.
    pub const ALL: [Verdict; 3] = [Verdict::Accept, Verdict::AcceptWithFixes, Verdict::Reject];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Verdict::Accept => "accept",
            Verdict::AcceptWithFixes => "accept_with_fixes",
            Verdict::Reject => "reject",
        }
    }

    /// Verdict implied by an overall score (`plan/06-memory-and-learning.md`
    /// §3.2): `≥ 0.75 → accept`, `≥ 0.5 → accept_with_fixes`, else `reject`.
    #[must_use]
    pub fn from_overall(overall: f32) -> Self {
        if overall >= 0.75 {
            Verdict::Accept
        } else if overall >= 0.5 {
            Verdict::AcceptWithFixes
        } else {
            Verdict::Reject
        }
    }

    /// The stricter (worse) of two verdicts.
    #[must_use]
    pub fn stricter(self, other: Verdict) -> Verdict {
        self.max(other)
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an evaluation judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum EvaluationSubject {
    /// A whole run.
    Run(RunId),
    /// A single task.
    Task(TaskId),
}

/// Kind of change an evaluation proposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    /// A prompt change.
    Prompt,
    /// A config change.
    Config,
    /// A routing change (candidate sets, resets, boosts).
    Routing,
}

/// Lifecycle of a proposal; only humans move it out of `Proposed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting in the inbox.
    #[default]
    Proposed,
    /// Accepted by a human.
    Accepted,
    /// Rejected by a human.
    Rejected,
}

/// A change proposed by the judge; never auto-applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// Proposal id.
    pub id: ProposalId,
    /// What it changes.
    pub kind: ProposalKind,
    /// The proposed change.
    pub body: String,
    /// Why.
    #[serde(default)]
    pub rationale: String,
    /// Inbox status.
    #[serde(default)]
    pub status: ProposalStatus,
}

// ---------------------------------------------------------------------------
// Goal, run mode
// ---------------------------------------------------------------------------

/// The user's original request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// Prompt text (trimmed).
    pub text: String,
    /// Attachments registered as artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ArtifactRef>,
    /// Working directory / repository root.
    pub cwd: PathBuf,
    /// Detected repository kind at `cwd`.
    #[serde(default)]
    pub repo_kind: RepoKind,
}

impl Goal {
    /// A goal with no attachments.
    #[must_use]
    pub fn new(text: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            text: text.into(),
            attachments: Vec::new(),
            cwd: cwd.into(),
            repo_kind: RepoKind::None,
        }
    }

    /// Sets the repository kind.
    #[must_use]
    pub const fn with_repo_kind(mut self, kind: RepoKind) -> Self {
        self.repo_kind = kind;
        self
    }
}

/// How a run interacts with humans.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunMode {
    /// Questions block, plans need approval.
    Interactive,
    /// Questions default/expire, plans auto-approved.
    Headless,
    /// One Kohral turn; never waits for a human.
    Kohral {
        /// Kohral turn id.
        turn_id: String,
        /// Kohral session key.
        session_key: String,
        /// Kohral session id.
        session_id: String,
    },
}

impl RunMode {
    /// `true` for [`RunMode::Interactive`].
    #[must_use]
    pub const fn is_interactive(&self) -> bool {
        matches!(self, RunMode::Interactive)
    }

    /// `true` for [`RunMode::Kohral`].
    #[must_use]
    pub const fn is_kohral(&self) -> bool {
        matches!(self, RunMode::Kohral { .. })
    }
}

/// Why a run failed (`run.failed.reason`). Known reasons have fixed
/// `snake_case` names; anything else round-trips through [`RunFailureReason::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RunFailureReason {
    /// A budget dimension was exhausted.
    BudgetExhausted,
    /// A question expired without a default.
    UnansweredQuestion,
    /// The planner produced an invalid plan twice.
    InvalidPlan,
    /// The plan was rejected more than `plan_revision_limit` times.
    PlanRevisionLimit,
    /// A required task failed permanently.
    TaskFailed,
    /// Integration (merge/PR) failed.
    IntegrationFailed,
    /// The runtime restarted under a Kohral turn.
    RuntimeRestarted,
    /// Free-form reason.
    Other(String),
}

impl RunFailureReason {
    /// The fixed reasons, in declaration order.
    pub const KNOWN: [RunFailureReason; 7] = [
        RunFailureReason::BudgetExhausted,
        RunFailureReason::UnansweredQuestion,
        RunFailureReason::InvalidPlan,
        RunFailureReason::PlanRevisionLimit,
        RunFailureReason::TaskFailed,
        RunFailureReason::IntegrationFailed,
        RunFailureReason::RuntimeRestarted,
    ];

    /// `snake_case` name (the raw text for `Other`).
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            RunFailureReason::BudgetExhausted => "budget_exhausted",
            RunFailureReason::UnansweredQuestion => "unanswered_question",
            RunFailureReason::InvalidPlan => "invalid_plan",
            RunFailureReason::PlanRevisionLimit => "plan_revision_limit",
            RunFailureReason::TaskFailed => "task_failed",
            RunFailureReason::IntegrationFailed => "integration_failed",
            RunFailureReason::RuntimeRestarted => "runtime_restarted",
            RunFailureReason::Other(s) => s,
        }
    }
}

impl fmt::Display for RunFailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunFailureReason {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(RunFailureReason::KNOWN
            .iter()
            .find(|r| r.as_str() == s)
            .cloned()
            .unwrap_or_else(|| RunFailureReason::Other(s.to_owned())))
    }
}

impl From<&str> for RunFailureReason {
    fn from(s: &str) -> Self {
        s.parse().unwrap_or_else(|never| match never {})
    }
}

impl Serialize for RunFailureReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RunFailureReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(RunFailureReason::from(
            String::deserialize(deserializer)?.as_str(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Task spec
// ---------------------------------------------------------------------------

/// What a task must do, as created by the orchestrator from a plan task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Short title (≤ 120 chars in plans).
    pub title: String,
    /// Full instructions for the worker.
    pub instructions: String,
    /// Input artifacts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<ArtifactRef>,
    /// Acceptance criteria the judge checks.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Tasks that must succeed first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TaskId>,
    /// Workspace preparation policy.
    #[serde(default)]
    pub workspace_policy: WorkspacePolicy,
    /// JSON schema the worker's final output must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<JsonSchema>,
    /// A failed optional task does not fail the run.
    #[serde(default)]
    pub optional: bool,
    /// May run concurrently with other tasks (default true).
    #[serde(default = "default_true")]
    pub parallel_safe: bool,
    /// The worker may push to the remote.
    #[serde(default)]
    pub allow_push: bool,
}

const fn default_true() -> bool {
    true
}

impl TaskSpec {
    /// A minimal spec with a title and instructions.
    #[must_use]
    pub fn new(title: impl Into<String>, instructions: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            instructions: instructions.into(),
            inputs: Vec::new(),
            acceptance_criteria: Vec::new(),
            depends_on: Vec::new(),
            workspace_policy: WorkspacePolicy::Isolated,
            output_schema: None,
            optional: false,
            parallel_safe: true,
            allow_push: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------

/// One selectable option of a question.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QuestionOption {
    /// Short label (what the user selects).
    pub label: String,
    /// Longer explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The planner's recommendation (used as default in headless modes).
    #[serde(default)]
    pub recommended: bool,
}

impl QuestionOption {
    /// An option with just a label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
            recommended: false,
        }
    }

    /// Marks the option as recommended.
    #[must_use]
    pub const fn recommended(mut self) -> Self {
        self.recommended = true;
        self
    }
}

/// An answer to a question.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Answer {
    /// Selected option labels.
    #[serde(default)]
    pub selected: Vec<String>,
    /// Free text when the options do not fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
    /// Who answered (`user name`, `"default"`, `"kohral"`).
    pub answered_by: String,
}

impl Answer {
    /// Name recorded in `answered_by` when a default was applied on expiry.
    pub const DEFAULT_ANSWERED_BY: &'static str = "default";

    /// An answer selecting the given labels.
    #[must_use]
    pub fn selected<I, S>(labels: I, answered_by: impl Into<String>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            selected: labels.into_iter().map(Into::into).collect(),
            free_text: None,
            answered_by: answered_by.into(),
        }
    }

    /// A free-text answer.
    #[must_use]
    pub fn free_text(text: impl Into<String>, answered_by: impl Into<String>) -> Self {
        Self {
            selected: Vec::new(),
            free_text: Some(text.into()),
            answered_by: answered_by.into(),
        }
    }

    /// `true` when nothing was selected and there is no free text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
            && self
                .free_text
                .as_deref()
                .is_none_or(|t| t.trim().is_empty())
    }
}

/// When an unanswered question stops blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionPolicy {
    /// Wait for a human indefinitely (interactive runs).
    Block,
    /// Apply the default after `timeout` (headless/Kohral runs).
    DefaultAfter {
        /// Timeout in milliseconds on the wire.
        #[serde(with = "duration_ms")]
        timeout: Duration,
    },
}

impl QuestionPolicy {
    /// Apply the default immediately.
    pub const IMMEDIATE_DEFAULT: QuestionPolicy = QuestionPolicy::DefaultAfter {
        timeout: Duration::ZERO,
    };

    /// `true` for [`QuestionPolicy::Block`].
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(self, QuestionPolicy::Block)
    }
}

/// Lifecycle of a question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    /// Waiting for an answer.
    #[default]
    Open,
    /// Answered (by a human or by the default).
    Answered,
    /// Expired without a default.
    Expired,
}

// ---------------------------------------------------------------------------
// Memory values
// ---------------------------------------------------------------------------

/// Kind of memory item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Actionable sentence learned from an evaluation.
    Lesson,
    /// A generalisable user preference.
    Preference,
    /// Operator-provided fact.
    Fact,
    /// Summary of a completed/failed run.
    RunSummary,
    /// Summary of an artifact.
    ArtifactSummary,
}

impl MemoryKind {
    /// Every kind.
    pub const ALL: [MemoryKind; 5] = [
        MemoryKind::Lesson,
        MemoryKind::Preference,
        MemoryKind::Fact,
        MemoryKind::RunSummary,
        MemoryKind::ArtifactSummary,
    ];

    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Lesson => "lesson",
            MemoryKind::Preference => "preference",
            MemoryKind::Fact => "fact",
            MemoryKind::RunSummary => "run_summary",
            MemoryKind::ArtifactSummary => "artifact_summary",
        }
    }

    /// Default importance per kind (`plan/06-memory-and-learning.md` §1.5).
    #[must_use]
    pub const fn default_importance(self) -> f32 {
        match self {
            MemoryKind::Lesson | MemoryKind::RunSummary => 0.5,
            MemoryKind::Preference => 0.8,
            MemoryKind::Fact => 0.6,
            MemoryKind::ArtifactSummary => 0.3,
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Provenance of a memory item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySource {
    /// Run it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// Task it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Evaluation it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_id: Option<EvaluationId>,
    /// Who wrote it.
    pub actor: Actor,
}

impl MemorySource {
    /// A source with only an actor.
    #[must_use]
    pub const fn from_actor(actor: Actor) -> Self {
        Self {
            run_id: None,
            task_id: None,
            evaluation_id: None,
            actor,
        }
    }
}

/// Visibility scope of a memory item: `global` or `repo:<canonical repo id>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum MemoryScope {
    /// Visible to every run.
    #[default]
    Global,
    /// Visible to runs on one repository.
    Repo(String),
}

/// Prefix of the string form of [`MemoryScope::Repo`].
pub const REPO_SCOPE_PREFIX: &str = "repo:";

impl fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryScope::Global => f.write_str("global"),
            MemoryScope::Repo(id) => write!(f, "{REPO_SCOPE_PREFIX}{id}"),
        }
    }
}

/// Error returned when a string is not a valid memory scope.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid memory scope {0:?}: expected `global` or `repo:<id>`")]
pub struct InvalidMemoryScope(pub String);

impl FromStr for MemoryScope {
    type Err = InvalidMemoryScope;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "global" {
            return Ok(MemoryScope::Global);
        }
        match s.strip_prefix(REPO_SCOPE_PREFIX) {
            Some(id) if !id.is_empty() => Ok(MemoryScope::Repo(id.to_owned())),
            _ => Err(InvalidMemoryScope(s.to_owned())),
        }
    }
}

impl Serialize for MemoryScope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MemoryScope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Validation error shared by value objects
// ---------------------------------------------------------------------------

/// A value object failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {field}: {reason}")]
pub struct InvalidValue {
    /// Field or value name.
    pub field: String,
    /// Human-readable reason.
    pub reason: String,
}

impl InvalidValue {
    /// Builds an error.
    pub fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use serde_json::json;

    use super::*;

    fn usd(s: &str) -> Decimal {
        s.parse().unwrap()
    }

    #[test]
    fn usage_is_additive_and_keeps_known_cost() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 1,
            cache_write_tokens: 2,
            cost_usd: Some(usd("0.5")),
            wall_ms: 100,
        };
        let b = Usage {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 1,
            cache_write_tokens: 1,
            cost_usd: None,
            wall_ms: 1,
        };
        let sum = a + b;
        assert_eq!(sum.input_tokens, 11);
        assert_eq!(sum.output_tokens, 6);
        assert_eq!(sum.cache_read_tokens, 2);
        assert_eq!(sum.cache_write_tokens, 3);
        assert_eq!(sum.cost_usd, Some(usd("0.5")));
        assert_eq!(sum.wall_ms, 101);
        assert_eq!(sum.total_tokens(), 17);
        let mut c = Usage::ZERO;
        c += a;
        c += a;
        assert_eq!(c.cost_usd, Some(usd("1.0")));
        assert_eq!([a, b, a].iter().sum::<Usage>().input_tokens, 21);
        assert!(Usage::ZERO.is_zero());
        assert!(!a.is_zero());
    }

    #[test]
    fn usage_add_saturates() {
        let a = Usage {
            input_tokens: u64::MAX,
            ..Usage::ZERO
        };
        assert_eq!((a + a).input_tokens, u64::MAX);
    }

    #[test]
    fn budget_defaults_and_exceeded() {
        let b = Budget::default();
        assert_eq!(b.max_attempts, 2);
        assert_eq!(b.max_parallel, 4);
        assert!(b.exceeded_by(&Usage::ZERO).is_none());
        let b = Budget::unlimited()
            .with_max_usd(usd("1.0"))
            .with_max_tokens(100);
        let under = Usage {
            input_tokens: 50,
            cost_usd: Some(usd("1.0")),
            ..Usage::ZERO
        };
        assert!(b.exceeded_by(&under).is_none());
        let over_usd = Usage {
            cost_usd: Some(usd("1.01")),
            ..Usage::ZERO
        };
        assert_eq!(
            b.exceeded_by(&over_usd),
            Some(BudgetExcess {
                dimension: BudgetDimension::Usd,
                limit: usd("1.0"),
                actual: usd("1.01"),
            })
        );
        let over_tokens = Usage {
            input_tokens: 60,
            output_tokens: 41,
            ..Usage::ZERO
        };
        assert_eq!(
            b.exceeded_by(&over_tokens).map(|e| e.dimension),
            Some(BudgetDimension::Tokens)
        );
        let wall = Budget::unlimited().with_max_wall(Duration::from_secs(60));
        assert!(wall.wall_exceeded_by(Duration::from_secs(60)).is_none());
        assert_eq!(
            wall.wall_exceeded_by(Duration::from_secs(61))
                .map(|e| e.dimension),
            Some(BudgetDimension::Wall)
        );
    }

    #[test]
    fn budget_serde_uses_millis_and_decimal_strings() {
        let b = Budget::unlimited()
            .with_max_usd(usd("2.50"))
            .with_max_wall(Duration::from_secs(90));
        let value = serde_json::to_value(&b).unwrap();
        assert_eq!(
            value,
            json!({
                "max_usd": "2.50",
                "max_tokens": null,
                "max_wall": 90_000,
                "max_attempts": 2,
                "max_parallel": 4
            })
        );
        let back: Budget = serde_json::from_value(value).unwrap();
        assert_eq!(back, b);
        let partial: Budget = serde_json::from_str("{}").unwrap();
        assert_eq!(partial, Budget::default());
    }

    #[test]
    fn route_display_and_serde() {
        let r = Route::new(WorkerKind::Claude, ModelAlias::new("opus5-claude").unwrap());
        assert_eq!(r.to_string(), "claude/opus5-claude");
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"worker":"claude","model":"opus5-claude"})
        );
        let r = r.with_effort(Effort::High);
        assert_eq!(r.to_string(), "claude/opus5-claude@high");
        let back: Route = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn workspace_and_run_mode_are_internally_tagged() {
        let ws = Workspace {
            root: "/tmp/x".into(),
            kind: WorkspaceKind::GitWorktree {
                branch: "kevin/t1".into(),
            },
            base_rev: Some("abc".into()),
        };
        assert_eq!(
            serde_json::to_value(&ws).unwrap(),
            json!({"root":"/tmp/x","kind":{"type":"git_worktree","branch":"kevin/t1"},"base_rev":"abc"})
        );
        assert_eq!(
            serde_json::to_value(Workspace::in_place("/r")).unwrap(),
            json!({"root":"/r","kind":{"type":"in_place"}})
        );
        assert_eq!(
            serde_json::to_value(RunMode::Headless).unwrap(),
            json!({"type":"headless"})
        );
        let k = RunMode::Kohral {
            turn_id: "t".into(),
            session_key: "k".into(),
            session_id: "s".into(),
        };
        assert!(k.is_kohral() && !k.is_interactive());
        let back: RunMode = serde_json::from_value(serde_json::to_value(&k).unwrap()).unwrap();
        assert_eq!(back, k);
    }

    #[test]
    fn rubric_score_validates_range() {
        assert!(RubricScore::new("correctness", 10, "ok").is_ok());
        assert!(RubricScore::new("correctness", 11, "ok").is_err());
        assert!(RubricScore::new(" ", 5, "ok").is_err());
    }

    #[test]
    fn verdict_thresholds_and_strictness() {
        assert_eq!(Verdict::from_overall(0.75), Verdict::Accept);
        assert_eq!(Verdict::from_overall(0.6), Verdict::AcceptWithFixes);
        assert_eq!(Verdict::from_overall(0.49), Verdict::Reject);
        assert_eq!(Verdict::Accept.stricter(Verdict::Reject), Verdict::Reject);
        assert_eq!(
            serde_json::to_string(&Verdict::AcceptWithFixes).unwrap(),
            "\"accept_with_fixes\""
        );
    }

    #[test]
    fn evaluation_subject_serde() {
        let id = RunId::nil();
        assert_eq!(
            serde_json::to_value(EvaluationSubject::Run(id)).unwrap(),
            json!({"type":"run","id":"00000000-0000-0000-0000-000000000000"})
        );
        let t = EvaluationSubject::Task(TaskId::nil());
        let back: EvaluationSubject =
            serde_json::from_value(serde_json::to_value(t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn run_failure_reason_round_trips_known_and_other() {
        for r in RunFailureReason::KNOWN {
            let json = serde_json::to_string(&r).unwrap();
            assert_eq!(json, format!("\"{r}\""));
            assert_eq!(serde_json::from_str::<RunFailureReason>(&json).unwrap(), r);
        }
        let other: RunFailureReason = serde_json::from_str("\"weird\"").unwrap();
        assert_eq!(other, RunFailureReason::Other("weird".into()));
        assert_eq!(
            RunFailureReason::from("task_failed"),
            RunFailureReason::TaskFailed
        );
    }

    #[test]
    fn question_policy_and_answer() {
        assert_eq!(
            serde_json::to_value(QuestionPolicy::Block).unwrap(),
            json!({"type":"block"})
        );
        assert_eq!(
            serde_json::to_value(QuestionPolicy::DefaultAfter {
                timeout: Duration::from_secs(600)
            })
            .unwrap(),
            json!({"type":"default_after","timeout":600_000})
        );
        assert_eq!(
            QuestionPolicy::IMMEDIATE_DEFAULT,
            QuestionPolicy::DefaultAfter {
                timeout: Duration::ZERO
            }
        );
        assert!(Answer::selected(Vec::<String>::new(), "v").is_empty());
        assert!(Answer::free_text("  ", "v").is_empty());
        assert!(!Answer::free_text("yes", "v").is_empty());
        assert!(!Answer::selected(["a"], "v").is_empty());
    }

    #[test]
    fn memory_scope_string_form() {
        assert_eq!(MemoryScope::Global.to_string(), "global");
        assert_eq!(MemoryScope::Repo("abc".into()).to_string(), "repo:abc");
        assert_eq!(
            "repo:abc".parse::<MemoryScope>().unwrap(),
            MemoryScope::Repo("abc".into())
        );
        assert!("repo:".parse::<MemoryScope>().is_err());
        assert!("local".parse::<MemoryScope>().is_err());
        assert_eq!(
            serde_json::to_string(&MemoryScope::Global).unwrap(),
            "\"global\""
        );
        assert!(serde_json::from_str::<MemoryScope>("\"nope\"").is_err());
        assert!((MemoryKind::Preference.default_importance() - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn task_spec_defaults() {
        let spec: TaskSpec =
            serde_json::from_value(json!({"title":"t","instructions":"i"})).unwrap();
        assert!(spec.parallel_safe);
        assert!(!spec.optional);
        assert_eq!(spec.workspace_policy, WorkspacePolicy::Isolated);
        assert_eq!(spec, TaskSpec::new("t", "i"));
    }
}
