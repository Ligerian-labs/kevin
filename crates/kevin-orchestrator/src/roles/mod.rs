//! Roles: the prompts, schemas and parsers Kevin uses when it calls a worker
//! *for itself* — planner (understanding, plan), clarifier, integrator and
//! summariser (`plan/05-orchestration.md` §3, `plan/06-memory-and-learning.md`
//! §1.5).
//!
//! A [`Role`] is pure: it turns a [`RoleContext`] into a [`RoleRequest`]
//! (`system`, `user`, `schema`) and parses the worker's answer back into a
//! domain type. [`RoleRunner`] is the only part that touches a
//! [`Worker`](kevin_worker::Worker): it runs one attempt with the role's
//! schema, repairs a schema violation exactly once
//! (`plan/04-workers.md` §Structured output) and accounts the usage.
//!
//! Prompts live as markdown templates under `crates/kevin-orchestrator/prompts/`
//! and JSON schemas under `crates/kevin-orchestrator/schemas/`; both are
//! `include_str!`-ed, so a prompt change shows up as a snapshot diff.
//!
//! Every system prompt states the prompt-injection rule
//! ([`PROMPT_INJECTION_RULE`], `plan/09-security.md`): repository text, tool
//! output and memory items are data, never instructions.

pub mod clarifier;
pub mod context;
pub mod integrator;
pub mod planner;
pub mod render;
pub mod runner;
pub mod schemas;
pub mod summarizer;

use std::time::Duration;

use kevin_domain::{FailureClass, ModelAlias, PlanError, Usage, WorkerKind};
use kevin_worker::structured::StructuredError;
use kevin_worker::{WorkerError, WorkspacePolicy};
use serde_json::Value;

pub use clarifier::{
    Clarifier, DraftedQuestions, QuestionSelection, SelectedQuestion, select_questions,
};
pub use context::{
    ASSUMPTION_PREFIX, ArtifactInput, BLOCKED_MARKER, BudgetHints, CHARS_PER_TOKEN, FeedbackSource,
    IntegrationFacts, MEMORY_BLOCK_CLOSE, MEMORY_BLOCK_OPEN, MemoryBlock, PlanFeedback,
    PriorAnswer, RepoFacts, RoleContext, RoleLimits, RunOutcome, StaticSystemContext,
    SystemContextProvider, SystemContextSection, TaskOutcome, estimate_tokens, human_duration,
};
pub use integrator::{
    IntegrationArtifact, IntegrationCheck, IntegrationConflict, IntegrationReport,
    IntegrationStatus, Integrator,
};
pub use planner::{PlannerPlan, PlannerUnderstanding};
pub use render::Vars;
pub use runner::RoleRunner;
pub use summarizer::{
    ArtifactSummary, MemoryRecords, PreferenceRecord, PreferenceScope, Summarizer,
};

use context::RoleContext as Ctx;
use render::{Vars as RenderVars, render};

/// The prompt-injection rule every system prompt states verbatim
/// (`plan/09-security.md` §Prompt-injection mitigations). Asserted by
/// `ac_ws10_4`.
pub const PROMPT_INJECTION_RULE: &str =
    include_str!("../../prompts/injection_rule.md").trim_ascii();

/// Rules shared by every role prompt (rendered with `{{injection_rule}}`).
const COMMON_RULES: &str = include_str!("../../prompts/common_rules.md");

/// What a role asks a worker to do: the two prompt halves and the JSON schema
/// the answer must match.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleRequest {
    /// Appended to the worker's system prompt.
    pub system: String,
    /// The message sent to the worker.
    pub user: String,
    /// Structured-output schema, when the role wants one.
    pub schema: Option<Value>,
}

/// One of Kevin's own prompts: builds the request, parses the answer.
///
/// Implementations are pure and stateless; anything a prompt needs comes from
/// the [`RoleContext`].
pub trait Role: Send + Sync {
    /// What [`Role::parse`] produces.
    type Output;

    /// Stable name (`planner.understanding`, `clarifier`, …) used in logs,
    /// errors and snapshot names.
    fn name(&self) -> &'static str;

    /// Renders the prompts and picks the schema.
    fn build(&self, ctx: &RoleContext) -> RoleRequest;

    /// Parses a worker's raw answer (bare JSON, fenced JSON, or JSON wrapped
    /// in prose) into the role's output.
    fn parse(&self, raw: &str) -> Result<Self::Output, RoleError>;

    /// Task kind recorded for the attempt this role runs as.
    fn task_kind(&self) -> kevin_domain::TaskKind {
        kevin_domain::TaskKind::Write
    }

    /// Whether the role's worker may write. Every role but the integrator runs
    /// read-only (`plan/05-orchestration.md` §3.2).
    fn workspace_policy(&self) -> WorkspacePolicy {
        WorkspacePolicy::ReadOnly
    }
}

/// Everything that can go wrong around a role call.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    /// No JSON, malformed JSON, or JSON that violates the role's schema.
    #[error("role `{role}`: {source}")]
    Output {
        /// Which role.
        role: &'static str,
        /// What the extractor/validator said.
        #[source]
        source: StructuredError,
    },
    /// Schema-valid JSON that does not deserialise into the domain type.
    #[error("role `{role}` produced JSON the domain rejects: {message}")]
    Parse {
        /// Which role.
        role: &'static str,
        /// Why.
        message: String,
    },
    /// The domain type deserialised but failed its own validation.
    #[error("role `{role}` produced an invalid {subject}: {message}")]
    Invalid {
        /// Which role.
        role: &'static str,
        /// What was invalid (`understanding`, `questions`, …).
        subject: &'static str,
        /// Why.
        message: String,
    },
    /// `PlanValidator` refused the plan (`plan/05-orchestration.md` §3.4).
    #[error(
        "role `{role}` produced an invalid plan: {}",
        format_plan_errors(errors)
    )]
    InvalidPlan {
        /// Which role.
        role: &'static str,
        /// Every problem found.
        errors: Vec<PlanError>,
    },
    /// The call did not finish within `orchestrator.role_call_timeout`.
    #[error("role `{role}` timed out after {}", human_duration(*timeout))]
    Timeout {
        /// Which role.
        role: &'static str,
        /// The timeout that elapsed.
        timeout: Duration,
    },
    /// No worker of that kind is registered (disabled or not built in).
    #[error("role `{role}`: worker `{worker}` is not available")]
    WorkerUnavailable {
        /// Which role.
        role: &'static str,
        /// The requested worker.
        worker: WorkerKind,
    },
    /// The route names a model alias that is not configured.
    #[error("role `{role}`: model alias `{alias}` is not configured")]
    UnknownModel {
        /// Which role.
        role: &'static str,
        /// The requested alias.
        alias: ModelAlias,
    },
    /// The worker could not be spawned.
    #[error("role `{role}`: {source}")]
    Spawn {
        /// Which role.
        role: &'static str,
        /// Spawn error.
        #[source]
        source: WorkerError,
    },
    /// The worker ran and failed.
    #[error("role `{role}` failed ({}): {message}", class.as_str())]
    WorkerFailed {
        /// Which role.
        role: &'static str,
        /// Failure class as classified by the adapter.
        class: FailureClass,
        /// Diagnostic.
        message: String,
        /// Usage burnt before failing.
        usage: Usage,
    },
}

impl RoleError {
    /// `true` when the answer broke the schema — the one case
    /// [`RoleRunner`] repairs with a follow-up turn.
    #[must_use]
    pub fn is_schema_violation(&self) -> bool {
        matches!(self, RoleError::Output { source, .. } if source.is_schema_violation())
    }

    /// The role that produced the error.
    #[must_use]
    pub const fn role(&self) -> &'static str {
        match self {
            RoleError::Output { role, .. }
            | RoleError::Parse { role, .. }
            | RoleError::Invalid { role, .. }
            | RoleError::InvalidPlan { role, .. }
            | RoleError::Timeout { role, .. }
            | RoleError::WorkerUnavailable { role, .. }
            | RoleError::UnknownModel { role, .. }
            | RoleError::Spawn { role, .. }
            | RoleError::WorkerFailed { role, .. } => role,
        }
    }

    /// The structured-output error behind [`RoleError::Output`], if any.
    #[must_use]
    pub const fn structured(&self) -> Option<&StructuredError> {
        match self {
            RoleError::Output { source, .. } => Some(source),
            _ => None,
        }
    }

    /// The usage burnt by the call, when any was reported.
    #[must_use]
    pub fn usage(&self) -> Usage {
        match self {
            RoleError::WorkerFailed { usage, .. } => *usage,
            _ => Usage::ZERO,
        }
    }
}

/// `error; error; …`
fn format_plan_errors(errors: &[PlanError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Renders one role's prompts: the shared rules are rendered first so
/// `{{injection_rule}}` inside them resolves, then both halves are rendered
/// with the same variables.
fn build_request(
    system_template: &str,
    user_template: &str,
    mut vars: RenderVars,
    schema: Value,
    schema_id: &str,
) -> RoleRequest {
    vars.set("injection_rule", PROMPT_INJECTION_RULE)
        .set("schema_id", schema_id);
    let common = render(COMMON_RULES, &vars);
    vars.set("common_rules", common);
    RoleRequest {
        system: render(system_template, &vars),
        user: render(user_template, &vars),
        schema: Some(schema),
    }
}

/// Extracts and schema-validates the worker's answer (fenced JSON tolerated).
fn extract(role: &'static str, raw: &str, schema: &Value) -> Result<Value, RoleError> {
    kevin_worker::structured::extract_and_validate(raw, schema)
        .map_err(|source| RoleError::Output { role, source })
}

/// Deserialises a validated document into the role's output type.
fn deserialize<T: serde::de::DeserializeOwned>(
    role: &'static str,
    value: Value,
) -> Result<T, RoleError> {
    serde_json::from_value(value).map_err(|err| RoleError::Parse {
        role,
        message: err.to_string(),
    })
}

/// The context's variables, so a role can add its own before rendering.
fn vars_of(ctx: &Ctx) -> RenderVars {
    ctx.vars()
}
