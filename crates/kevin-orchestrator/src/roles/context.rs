//! [`RoleContext`] — everything a [`Role`](super::Role) may put in a prompt,
//! and the value objects it is built from (`plan/05-orchestration.md` §3,
//! `plan/06-memory-and-learning.md` §1.6).
//!
//! A context is a plain data structure: building it is the saga's job (WS-08),
//! rendering it is the role's. Nothing here performs IO, so a prompt is a pure
//! function of its context and every prompt can be snapshotted.

use std::fmt;
use std::fmt::Write as _;
use std::time::Duration;

use kevin_config::{Memory as MemoryCfg, Orchestrator as OrchestratorCfg};
use kevin_domain::plan::Plan;
use kevin_domain::{
    Answer, ArtifactKind, Budget, Decimal, PlanError, RepoKind, RunMode, Understanding, Usage,
};

use super::render::Vars;

/// Characters per token used by every estimate in Kevin
/// (`plan/06-memory-and-learning.md` §1.6).
pub const CHARS_PER_TOKEN: usize = 4;

/// Opening tag of the memory context block.
pub const MEMORY_BLOCK_OPEN: &str = "<kevin-memory>";

/// Closing tag of the memory context block.
pub const MEMORY_BLOCK_CLOSE: &str = "</kevin-memory>";

/// Prefix of an assumption derived from a question Kevin decided not to ask
/// (`plan/05-orchestration.md` §3.2).
pub const ASSUMPTION_PREFIX: &str = "Assumed: ";

/// Marker Kohral writes over a file it considers prompt-injected; such a file
/// is treated as missing (`plan/08-kohral-runtime.md` §5.1).
pub const BLOCKED_MARKER: &str = "[BLOCKED";

/// Rough token count of `text` (4 characters per token, as everywhere else).
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(CHARS_PER_TOKEN)
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// The `[orchestrator]` and `[memory]` knobs a prompt or a selection rule
/// needs (`plan/03-config-schema.md`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RoleLimits {
    /// `orchestrator.question_confidence_threshold`.
    pub question_confidence_threshold: f32,
    /// `orchestrator.max_questions_per_run`.
    pub max_questions_per_run: usize,
    /// `orchestrator.max_tasks_per_run`.
    pub max_tasks_per_run: usize,
    /// `orchestrator.question_default_timeout`.
    pub question_default_timeout: Duration,
    /// `orchestrator.role_call_timeout`.
    pub role_call_timeout: Duration,
    /// `orchestrator.plan_revision_limit`.
    pub plan_revision_limit: u32,
    /// `memory.context_max_tokens`.
    pub memory_context_max_tokens: usize,
}

impl Default for RoleLimits {
    /// The defaults of `plan/03-config-schema.md`.
    fn default() -> Self {
        Self {
            question_confidence_threshold: 0.7,
            max_questions_per_run: 4,
            max_tasks_per_run: 24,
            question_default_timeout: Duration::from_mins(10),
            role_call_timeout: Duration::from_mins(15),
            plan_revision_limit: 2,
            memory_context_max_tokens: 2500,
        }
    }
}

impl RoleLimits {
    /// Reads the limits from the resolved configuration.
    ///
    /// The confidence threshold is narrowed to `f32`, the width
    /// [`ProposedQuestion::confidence_if_unasked`](kevin_domain::ProposedQuestion)
    /// uses; the lost precision is far below anything a planner reports.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn from_config(orchestrator: &OrchestratorCfg, memory: &MemoryCfg) -> Self {
        Self {
            question_confidence_threshold: orchestrator.question_confidence_threshold as f32,
            max_questions_per_run: orchestrator.max_questions_per_run as usize,
            max_tasks_per_run: orchestrator.max_tasks_per_run as usize,
            question_default_timeout: orchestrator.question_default_timeout,
            role_call_timeout: orchestrator.role_call_timeout,
            plan_revision_limit: orchestrator.plan_revision_limit,
            memory_context_max_tokens: memory.context_max_tokens as usize,
        }
    }
}

// ---------------------------------------------------------------------------
// Repository facts
// ---------------------------------------------------------------------------

/// What the orchestrator knows about the repository the run targets. Every
/// field is *data*, never an instruction (`plan/09-security.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoFacts {
    /// Repository name (directory or remote name).
    pub name: String,
    /// Absolute root, for display only.
    pub root: String,
    /// Detected version-control flavour.
    pub vcs: RepoKind,
    /// Branch/bookmark the run integrates onto.
    pub base_branch: Option<String>,
    /// Top-level entries (`plan/05-orchestration.md` §3.1 repo summary).
    pub top_level: Vec<String>,
    /// Detected languages/toolchains.
    pub languages: Vec<String>,
    /// `.kevin/kevin.toml [checks] commands`.
    pub checks: Vec<String>,
    /// Anything else worth one line.
    pub notes: Vec<String>,
}

impl RepoFacts {
    /// Facts for a repository with only a name and a root.
    #[must_use]
    pub fn new(name: impl Into<String>, root: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            root: root.into(),
            ..Self::default()
        }
    }

    fn render(&self) -> String {
        let mut lines = vec![
            format!("- name: {}", self.name),
            format!("- root: {}", self.root),
            format!(
                "- vcs: {}",
                match self.vcs {
                    RepoKind::Git => "git",
                    RepoKind::Jj => "jj",
                    RepoKind::None => "none",
                }
            ),
        ];
        if let Some(branch) = &self.base_branch {
            lines.push(format!("- base branch: {branch}"));
        }
        if !self.languages.is_empty() {
            lines.push(format!("- languages: {}", self.languages.join(", ")));
        }
        if !self.top_level.is_empty() {
            lines.push(format!("- top level: {}", self.top_level.join(", ")));
        }
        if !self.checks.is_empty() {
            lines.push(format!("- declared checks: {}", self.checks.join(" && ")));
        }
        for note in &self.notes {
            lines.push(format!("- {note}"));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Prior answers
// ---------------------------------------------------------------------------

/// A question a human (or a default) already answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorAnswer {
    /// The question as it was asked.
    pub question: String,
    /// What was chosen, joined for display.
    pub answer: String,
    /// `answered_by` of the domain [`Answer`].
    pub answered_by: String,
}

impl PriorAnswer {
    /// Builds a prior answer from the domain [`Answer`].
    #[must_use]
    pub fn new(question: impl Into<String>, answer: &Answer) -> Self {
        let mut text = answer.selected.join(", ");
        if let Some(free) = &answer.free_text {
            if text.is_empty() {
                text.clone_from(free);
            } else {
                text = format!("{text} — {free}");
            }
        }
        Self {
            question: question.into(),
            answer: text,
            answered_by: answer.answered_by.clone(),
        }
    }

    fn render(&self) -> String {
        format!(
            "- {} → {} (answered by {})",
            self.question, self.answer, self.answered_by
        )
    }
}

// ---------------------------------------------------------------------------
// Memory block
// ---------------------------------------------------------------------------

/// The `<kevin-memory>` block injected into planner/worker context, capped at
/// `memory.context_max_tokens` (`plan/06-memory-and-learning.md` §1.6).
///
/// Items are rendered by `kevin-memory` in descending score order, so capping
/// drops the lowest-scored ones: item lines (`- …`) are removed from the end
/// until the block fits, and a note records how many were omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryBlock {
    text: String,
    max_tokens: usize,
    dropped: usize,
}

impl MemoryBlock {
    /// Wraps `items` in the memory tags and caps the result at `max_tokens`.
    /// An already wrapped block is accepted (the tags are not doubled).
    #[must_use]
    pub fn new(items: &str, max_tokens: usize) -> Self {
        let body = items
            .trim()
            .trim_start_matches(MEMORY_BLOCK_OPEN)
            .trim_end_matches(MEMORY_BLOCK_CLOSE)
            .trim();
        let (text, dropped) = cap_block(body, max_tokens);
        Self {
            text,
            max_tokens,
            dropped,
        }
    }

    /// The rendered, capped block including its tags.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The configured cap.
    #[must_use]
    pub const fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    /// Estimated tokens of the rendered block (never above [`Self::max_tokens`]).
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_tokens(&self.text)
    }

    /// How many item lines the cap dropped.
    #[must_use]
    pub const fn dropped_items(&self) -> usize {
        self.dropped
    }

    /// `true` when the block carries no item at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.text.lines().any(|l| l.trim_start().starts_with("- "))
    }
}

impl fmt::Display for MemoryBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Room reserved for the omission note while dropping items (its exact length
/// depends on the count).
const NOTE_RESERVE: usize = 96;

/// Wraps `body` and drops item lines from the end until it fits `max_tokens`.
fn cap_block(body: &str, max_tokens: usize) -> (String, usize) {
    let max_chars = max_tokens.saturating_mul(CHARS_PER_TOKEN);
    let lines: Vec<&str> = body.lines().collect();
    let is_item = |line: &&str| line.trim_start().starts_with("- ");
    let wrapper = MEMORY_BLOCK_OPEN.chars().count() + MEMORY_BLOCK_CLOSE.chars().count() + 2;

    let mut kept = lines.len();
    let mut chars: usize = wrapper + lines.iter().map(|l| l.chars().count() + 1).sum::<usize>();
    let mut dropped = 0;
    while chars > max_chars {
        let Some(pos) = lines[..kept].iter().rposition(is_item) else {
            break;
        };
        chars = chars.saturating_sub(lines[pos].chars().count() + 1);
        kept = pos;
        if dropped == 0 {
            chars += NOTE_RESERVE;
        }
        dropped += 1;
    }

    let mut out = String::from(MEMORY_BLOCK_OPEN);
    for line in &lines[..kept] {
        out.push('\n');
        out.push_str(line);
    }
    if dropped > 0 {
        let _ = write!(
            out,
            "\n- […] {dropped} lower-scored item(s) omitted (memory.context_max_tokens = {max_tokens})"
        );
    }
    out.push('\n');
    out.push_str(MEMORY_BLOCK_CLOSE);

    if estimate_tokens(&out) > max_tokens {
        // Nothing left to drop (headings only, or a cap smaller than the tags):
        // hard-truncate the body, keeping the tags intact.
        let budget = max_chars.saturating_sub(
            MEMORY_BLOCK_OPEN.chars().count() + MEMORY_BLOCK_CLOSE.chars().count() + 2,
        );
        let body: String = out
            .trim_start_matches(MEMORY_BLOCK_OPEN)
            .trim_end_matches(MEMORY_BLOCK_CLOSE)
            .trim()
            .chars()
            .take(budget)
            .collect();
        out = format!(
            "{MEMORY_BLOCK_OPEN}\n{}\n{MEMORY_BLOCK_CLOSE}",
            body.trim_end()
        );
    }
    (out, dropped)
}

// ---------------------------------------------------------------------------
// Budget hints
// ---------------------------------------------------------------------------

/// Budget and cost hints shown to a role so it can size its answer
/// (`plan/05-orchestration.md` §4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetHints {
    /// `budget.max_usd`.
    pub max_usd: Option<Decimal>,
    /// `budget.max_tokens`.
    pub max_tokens: Option<u64>,
    /// `budget.max_wall`.
    pub max_wall: Option<Duration>,
    /// `budget.max_parallel`.
    pub max_parallel: Option<u16>,
    /// Spend recorded on the run so far.
    pub spent_usd: Option<Decimal>,
}

impl From<&Budget> for BudgetHints {
    fn from(budget: &Budget) -> Self {
        Self {
            max_usd: budget.max_usd,
            max_tokens: budget.max_tokens,
            max_wall: budget.max_wall,
            max_parallel: Some(budget.max_parallel),
            spent_usd: None,
        }
    }
}

impl BudgetHints {
    /// Records what the run already spent.
    #[must_use]
    pub const fn with_spent_usd(mut self, spent: Decimal) -> Self {
        self.spent_usd = Some(spent);
        self
    }

    fn render(&self) -> String {
        let mut lines = Vec::new();
        if let Some(max) = self.max_usd {
            match self.spent_usd {
                Some(spent) => lines.push(format!("- budget: {max} USD (spent so far: {spent})")),
                None => lines.push(format!("- budget: {max} USD")),
            }
        } else if let Some(spent) = self.spent_usd {
            lines.push(format!("- spent so far: {spent} USD"));
        }
        if let Some(tokens) = self.max_tokens {
            lines.push(format!("- token budget: {tokens}"));
        }
        if let Some(wall) = self.max_wall {
            lines.push(format!("- wall-clock budget: {}", human_duration(wall)));
        }
        if let Some(parallel) = self.max_parallel {
            lines.push(format!("- at most {parallel} task attempts run at once"));
        }
        lines.join("\n")
    }
}

/// `1h 30m`, `15m`, `12s`, `0s`.
#[must_use]
pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Plan feedback (revision loop)
// ---------------------------------------------------------------------------

/// Who rejected a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSource {
    /// A human pressed *reject* with feedback (`RejectPlan{feedback}`).
    Human,
    /// `PlanValidator` refused the plan (`plan/05-orchestration.md` §3.4).
    Validator,
}

impl FeedbackSource {
    /// Lowercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            FeedbackSource::Human => "human",
            FeedbackSource::Validator => "validator",
        }
    }
}

/// One round of the plan revision loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFeedback {
    /// 1 for the first rejection.
    pub revision: u32,
    /// Who produced it.
    pub source: FeedbackSource,
    /// What has to change (one entry per point).
    pub points: Vec<String>,
}

impl PlanFeedback {
    /// A human rejection.
    #[must_use]
    pub fn rejected(revision: u32, feedback: impl Into<String>) -> Self {
        Self {
            revision,
            source: FeedbackSource::Human,
            points: vec![feedback.into()],
        }
    }

    /// The validator's errors, one point per error.
    #[must_use]
    pub fn validation(revision: u32, errors: &[PlanError]) -> Self {
        Self {
            revision,
            source: FeedbackSource::Validator,
            points: errors.iter().map(ToString::to_string).collect(),
        }
    }

    fn render(&self) -> String {
        let mut lines = vec![format!(
            "Revision {} — rejected by the {}:",
            self.revision,
            self.source.as_str()
        )];
        lines.extend(self.points.iter().map(|p| format!("- {p}")));
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Task / run outcomes and artifacts
// ---------------------------------------------------------------------------

/// What one task of the run ended up doing (integrator and summariser input).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskOutcome {
    /// Plan-local id (`t1`).
    pub id: String,
    /// Task title.
    pub title: String,
    /// Task kind.
    pub kind: String,
    /// Terminal status (`succeeded`, `failed`, `skipped`, `cancelled`).
    pub status: String,
    /// Branch/bookmark holding the work, when the workspace produced one.
    pub branch: Option<String>,
    /// The attempt's own summary.
    pub summary: String,
}

impl TaskOutcome {
    fn render(&self) -> String {
        let branch = self
            .branch
            .as_ref()
            .map_or_else(|| "no branch".to_owned(), |b| format!("branch {b}"));
        format!(
            "- {} [{}] {} — {} ({})\n  {}",
            self.id, self.kind, self.title, self.status, branch, self.summary
        )
    }
}

/// How the run ended (summariser input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// `completed`, `failed`, `cancelled`.
    pub status: String,
    /// Wall-clock time of the run.
    pub duration: Duration,
    /// Usage rolled up from the tasks.
    pub usage: Usage,
    /// `run.failed.reason` when it failed.
    pub failure_reason: Option<String>,
}

impl RunOutcome {
    fn render(&self) -> String {
        let mut lines = vec![
            format!("- status: {}", self.status),
            format!("- duration: {}", human_duration(self.duration)),
            format!(
                "- tokens: {} in / {} out",
                self.usage.input_tokens, self.usage.output_tokens
            ),
        ];
        if let Some(cost) = self.usage.cost_usd {
            lines.push(format!("- cost: {cost} USD"));
        }
        if let Some(reason) = &self.failure_reason {
            lines.push(format!("- failure reason: {reason}"));
        }
        lines.join("\n")
    }
}

/// An artifact the summariser must describe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactInput {
    /// Stable id echoed back in `artifact_summaries[].artifact_id`.
    pub id: String,
    /// What it is.
    pub kind: ArtifactKind,
    /// Where it lives.
    pub uri: String,
    /// One line of provenance.
    pub description: String,
}

impl ArtifactInput {
    fn render(&self) -> String {
        format!(
            "- {} [{}] {} — {}",
            self.id,
            serde_json::to_value(self.kind)
                .ok()
                .and_then(|v| v.as_str().map(ToOwned::to_owned))
                .unwrap_or_default(),
            self.uri,
            self.description
        )
    }
}

/// Integration settings and state (`plan/05-orchestration.md` §3.6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntegrationFacts {
    /// `workspace.integration`: `pr`, `merge` or `none`.
    pub mode: String,
    /// Branch/bookmark to integrate onto.
    pub base_branch: String,
    /// `workspace.pr_per_task`.
    pub pr_per_task: bool,
    /// Commands from `.kevin/kevin.toml [checks]`.
    pub checks: Vec<String>,
    /// Conflicts already known (from a previous integration attempt).
    pub conflicts: Vec<String>,
}

impl IntegrationFacts {
    fn render(&self) -> String {
        let mut lines = vec![
            format!("- mode: {}", self.mode),
            format!("- base branch: {}", self.base_branch),
            format!(
                "- pull requests: {}",
                if self.pr_per_task {
                    "one per succeeded task"
                } else {
                    "one for the whole run"
                }
            ),
        ];
        if self.checks.is_empty() {
            lines.push("- declared checks: none".to_owned());
        } else {
            lines.push(format!("- declared checks: {}", self.checks.join(" && ")));
        }
        for conflict in &self.conflicts {
            lines.push(format!("- known conflict: {conflict}"));
        }
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// System context (platform briefing)
// ---------------------------------------------------------------------------

/// One titled block of platform context prepended to a role's system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemContextSection {
    /// Short title (`AGENTS.md`, `SOUL.md`, …).
    pub title: String,
    /// Verbatim body.
    pub body: String,
}

impl SystemContextSection {
    /// A section, unless the body is blank or was blocked by the platform's
    /// own prompt-injection scan (`plan/08-kohral-runtime.md` §5.1); a blocked
    /// file is treated as missing and logged.
    #[must_use]
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Option<Self> {
        let title = title.into();
        let body = body.into();
        if body.trim().is_empty() {
            return None;
        }
        if body.trim_start().starts_with(BLOCKED_MARKER) {
            tracing::warn!(
                section = %title,
                "system context section was blocked by the platform; treating it as missing"
            );
            return None;
        }
        Some(Self { title, body })
    }

    fn render(&self) -> String {
        format!("## {}\n\n{}", self.title, self.body.trim())
    }
}

/// Hook letting a platform (Kohral, WS-22) prepend a briefing to every role
/// prompt (`plan/08-kohral-runtime.md` §5.1). Implementations must be pure and
/// cheap: `sections` is called for every prompt build.
pub trait SystemContextProvider: fmt::Debug + Send + Sync {
    /// Name of the provider (`kohral`, …), for logs.
    fn name(&self) -> &'static str;

    /// Sections in the order they must appear.
    fn sections(&self) -> Vec<SystemContextSection>;
}

/// A [`SystemContextProvider`] over a fixed list of sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSystemContext {
    name: &'static str,
    sections: Vec<SystemContextSection>,
}

impl StaticSystemContext {
    /// Builds the provider, skipping blank and `[BLOCKED …]` bodies.
    pub fn new<I, T, B>(name: &'static str, sections: I) -> Self
    where
        I: IntoIterator<Item = (T, B)>,
        T: Into<String>,
        B: Into<String>,
    {
        Self {
            name,
            sections: sections
                .into_iter()
                .filter_map(|(t, b)| SystemContextSection::new(t, b))
                .collect(),
        }
    }
}

impl SystemContextProvider for StaticSystemContext {
    fn name(&self) -> &'static str {
        self.name
    }

    fn sections(&self) -> Vec<SystemContextSection> {
        self.sections.clone()
    }
}

// ---------------------------------------------------------------------------
// RoleContext
// ---------------------------------------------------------------------------

/// Everything a role may need to build its prompt. Roles read only the parts
/// they need; the saga fills in what it has.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleContext {
    /// The run's goal text.
    pub goal: String,
    /// Interactive / headless / Kohral.
    pub run_mode: Option<RunMode>,
    /// Facts about the target repository.
    pub repo: RepoFacts,
    /// Questions already answered.
    pub prior_answers: Vec<PriorAnswer>,
    /// The capped `<kevin-memory>` block.
    pub memory: Option<MemoryBlock>,
    /// Criteria the whole run must satisfy.
    pub acceptance_criteria: Vec<String>,
    /// Budget and cost hints.
    pub budget: BudgetHints,
    /// Rejections of previous plans, oldest first.
    pub plan_feedback: Vec<PlanFeedback>,
    /// The recorded understanding, once the phase produced one.
    pub understanding: Option<Understanding>,
    /// The approved plan, once there is one.
    pub plan: Option<Plan>,
    /// Terminal task outcomes.
    pub tasks: Vec<TaskOutcome>,
    /// Artifacts to summarise.
    pub artifacts: Vec<ArtifactInput>,
    /// Integration settings.
    pub integration: IntegrationFacts,
    /// How the run ended.
    pub run_outcome: Option<RunOutcome>,
    /// Configured limits.
    pub limits: RoleLimits,
    /// Platform briefing sections, in order.
    pub system_context: Vec<SystemContextSection>,
}

impl RoleContext {
    /// A context for `goal` with default limits and nothing else.
    #[must_use]
    pub fn new(goal: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            limits: RoleLimits::default(),
            ..Self::default()
        }
    }

    /// Sets the run mode.
    #[must_use]
    pub fn with_run_mode(mut self, mode: RunMode) -> Self {
        self.run_mode = Some(mode);
        self
    }

    /// Sets the repository facts.
    #[must_use]
    pub fn with_repo(mut self, repo: RepoFacts) -> Self {
        self.repo = repo;
        self
    }

    /// Sets the configured limits.
    #[must_use]
    pub const fn with_limits(mut self, limits: RoleLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Sets the answers already given.
    #[must_use]
    pub fn with_prior_answers<I: IntoIterator<Item = PriorAnswer>>(mut self, answers: I) -> Self {
        self.prior_answers = answers.into_iter().collect();
        self
    }

    /// Sets the memory block (already capped by [`MemoryBlock::new`]).
    #[must_use]
    pub fn with_memory(mut self, memory: MemoryBlock) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Sets the run-level acceptance criteria.
    #[must_use]
    pub fn with_acceptance_criteria<I, S>(mut self, criteria: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.acceptance_criteria = criteria.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the budget hints.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetHints) -> Self {
        self.budget = budget;
        self
    }

    /// Sets the plan revision feedback.
    #[must_use]
    pub fn with_plan_feedback<I: IntoIterator<Item = PlanFeedback>>(mut self, feedback: I) -> Self {
        self.plan_feedback = feedback.into_iter().collect();
        self
    }

    /// Sets the recorded understanding.
    #[must_use]
    pub fn with_understanding(mut self, understanding: Understanding) -> Self {
        self.understanding = Some(understanding);
        self
    }

    /// Sets the plan.
    #[must_use]
    pub fn with_plan(mut self, plan: Plan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Sets the task outcomes.
    #[must_use]
    pub fn with_tasks<I: IntoIterator<Item = TaskOutcome>>(mut self, tasks: I) -> Self {
        self.tasks = tasks.into_iter().collect();
        self
    }

    /// Sets the artifacts to summarise.
    #[must_use]
    pub fn with_artifacts<I: IntoIterator<Item = ArtifactInput>>(mut self, artifacts: I) -> Self {
        self.artifacts = artifacts.into_iter().collect();
        self
    }

    /// Sets the integration settings.
    #[must_use]
    pub fn with_integration(mut self, integration: IntegrationFacts) -> Self {
        self.integration = integration;
        self
    }

    /// Sets the run outcome.
    #[must_use]
    pub fn with_run_outcome(mut self, outcome: RunOutcome) -> Self {
        self.run_outcome = Some(outcome);
        self
    }

    /// Appends the provider's sections (blocked and blank ones are skipped).
    #[must_use]
    pub fn with_system_context(mut self, provider: &dyn SystemContextProvider) -> Self {
        let sections = provider.sections();
        tracing::debug!(
            provider = provider.name(),
            sections = sections.len(),
            "system context injected into role prompts"
        );
        self.system_context.extend(sections);
        self
    }

    /// The run mode's `snake_case` name (`interactive` when unset).
    #[must_use]
    pub fn run_mode_name(&self) -> &'static str {
        match self.run_mode {
            Some(RunMode::Headless) => "headless",
            Some(RunMode::Kohral { .. }) => "kohral",
            Some(RunMode::Interactive) | None => "interactive",
        }
    }

    /// One sentence telling the model what the mode implies for questions and
    /// plan approval (`plan/05-orchestration.md` §5).
    #[must_use]
    pub fn run_mode_note(&self) -> &'static str {
        match self.run_mode {
            Some(RunMode::Headless) => {
                "no human is watching, so an unanswered question falls back to its recommended \
                 option after a timeout and the plan is approved automatically."
            }
            Some(RunMode::Kohral { .. }) => {
                "one autonomous turn on the Kohral platform; nobody can be asked anything, so a \
                 question without a recommended option must become an assumption instead."
            }
            Some(RunMode::Interactive) | None => {
                "a human is available, so questions block the run and the plan needs approval."
            }
        }
    }

    /// The variables every prompt template may use.
    pub(super) fn vars(&self) -> Vars {
        let mut vars = Vars::new();
        vars.set("goal", self.goal.trim())
            .set("repo", self.repo.render())
            .set("run_mode", self.run_mode_name())
            .set("run_mode_note", self.run_mode_note())
            .set("assumption_prefix", ASSUMPTION_PREFIX)
            .set(
                "max_questions_per_run",
                self.limits.max_questions_per_run.to_string(),
            )
            .set(
                "max_tasks_per_run",
                self.limits.max_tasks_per_run.to_string(),
            )
            .set(
                "max_proposed_questions",
                kevin_domain::understanding::MAX_PROPOSED_QUESTIONS.to_string(),
            )
            .set(
                "question_confidence_threshold",
                format!("{:.2}", self.limits.question_confidence_threshold),
            )
            .set("task_kinds", kevin_domain::plan::PLAN_TASK_KINDS.join(", "))
            .set("integration_mode", &self.integration.mode)
            .set("base_branch", &self.integration.base_branch);
        vars.set_opt("memory", self.memory.as_ref().map(MemoryBlock::text));
        vars.set_lines(
            "prior_answers",
            self.prior_answers.iter().map(PriorAnswer::render),
        );
        vars.set_lines(
            "acceptance_criteria",
            self.acceptance_criteria.iter().map(|c| format!("- {c}")),
        );
        vars.set_opt("budget", Some(self.budget.render()));
        vars.set_lines(
            "plan_feedback",
            self.plan_feedback
                .iter()
                .map(PlanFeedback::render)
                .collect::<Vec<_>>()
                .join("\n\n")
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
        );
        vars.set_opt(
            "understanding",
            self.understanding.as_ref().map(render_understanding),
        );
        vars.set_opt("plan", self.plan.as_ref().map(render_plan));
        vars.set_lines("tasks", self.tasks.iter().map(TaskOutcome::render));
        vars.set_lines(
            "artifacts",
            self.artifacts.iter().map(ArtifactInput::render),
        );
        vars.set_opt("integration", Some(self.integration.render()));
        vars.set_opt(
            "run_outcome",
            self.run_outcome.as_ref().map(RunOutcome::render),
        );
        vars.set_lines(
            "system_context",
            self.system_context
                .iter()
                .map(SystemContextSection::render)
                .collect::<Vec<_>>()
                .join("\n\n")
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
        );
        vars
    }
}

/// The understanding as markdown (the planner reads prose better than JSON).
fn render_understanding(u: &Understanding) -> String {
    let mut lines = vec![
        format!("Objective: {}", u.objective),
        format!("Complexity: {}", u.complexity),
    ];
    let bullets = |title: &str, items: &[String], lines: &mut Vec<String>| {
        if !items.is_empty() {
            lines.push(format!("{title}:"));
            lines.extend(items.iter().map(|i| format!("- {i}")));
        }
    };
    bullets("Success criteria", &u.success_criteria, &mut lines);
    bullets("Assumptions", &u.assumptions, &mut lines);
    bullets("Risks", &u.risks, &mut lines);
    if !u.suggested_task_kinds.is_empty() {
        lines.push(format!(
            "Suggested task kinds: {}",
            u.suggested_task_kinds.join(", ")
        ));
    }
    if !u.context_refs.is_empty() {
        lines.push(format!("Context refs: {}", u.context_refs.join(", ")));
    }
    lines.join("\n")
}

/// The plan as markdown.
fn render_plan(plan: &Plan) -> String {
    let mut lines = vec![format!("Rationale: {}", plan.rationale)];
    for task in &plan.tasks {
        let deps = plan.dependencies_of(&task.id);
        lines.push(format!(
            "- {} [{}] {} — depends on: {}",
            task.id,
            task.kind,
            task.title,
            if deps.is_empty() {
                "nothing".to_owned()
            } else {
                deps.join(", ")
            }
        ));
        for criterion in &task.acceptance_criteria {
            lines.push(format!("  - acceptance: {criterion}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_block_wraps_and_reports_no_drop_when_it_fits() {
        let block = MemoryBlock::new("Lessons:\n- [L-1] fmt first", 2500);
        assert!(block.text().starts_with(MEMORY_BLOCK_OPEN));
        assert!(block.text().ends_with(MEMORY_BLOCK_CLOSE));
        assert_eq!(block.dropped_items(), 0);
        assert!(!block.is_empty());
        // Wrapping an already wrapped block does not double the tags.
        let again = MemoryBlock::new(block.text(), 2500);
        assert_eq!(again.text(), block.text());
    }

    #[test]
    fn memory_block_hard_truncates_when_there_is_nothing_left_to_drop() {
        let block = MemoryBlock::new("Lessons about a very long heading with no items at all", 12);
        assert!(block.estimated_tokens() <= 12, "{}", block.text());
        assert!(block.text().starts_with(MEMORY_BLOCK_OPEN));
        assert!(block.text().ends_with(MEMORY_BLOCK_CLOSE));
        assert!(block.is_empty());
    }

    #[test]
    fn blocked_system_context_sections_are_dropped() {
        assert!(SystemContextSection::new("SOUL.md", "[BLOCKED: injection]").is_none());
        assert!(SystemContextSection::new("SOUL.md", "   ").is_none());
        assert!(SystemContextSection::new("SOUL.md", "Be terse.").is_some());
    }

    #[test]
    fn human_duration_reads_like_a_human_wrote_it() {
        assert_eq!(human_duration(Duration::from_secs(0)), "0s");
        assert_eq!(human_duration(Duration::from_secs(930)), "15m 30s");
        assert_eq!(human_duration(Duration::from_secs(7200)), "2h");
    }
}
