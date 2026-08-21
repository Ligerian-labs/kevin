//! Ports the orchestration engine depends on.
//!
//! WS-08 landed before `kevin-router` (WS-09), `kevin-orchestrator::roles`
//! (WS-10), `kevin-memory` (WS-18) and `kevin-evaluator` (WS-19). The engine
//! therefore codes against the traits below — shaped after the frozen APIs in
//! `plan/06-memory-and-learning.md` §2/§3 and `plan/12-workstreams.md` — and
//! **WS-12 wires the real crates into [`crate::Deps`]**. `kevin-testkit`-style
//! fakes for every port live in [`crate::testing`].
//!
//! WS-10 (`roles`) and WS-11 (`projections`) are merged: [`RolesPort`] is
//! implemented over their `RoleRunner` in [`crate::role_port`], and the
//! system-context hook is WS-10's own
//! [`SystemContextProvider`](crate::roles::SystemContextProvider).
//!
//! Two more seams are ports for a different reason:
//!
//! - [`WorkspacePort`] hides `kevin-workspace`'s blocking API (git/jj/gh
//!   subprocesses) behind an async trait so the engine never blocks a reactor
//!   thread and tests need no repository.
//! - [`CommandIdempotency`] hides `kevin_store::CommandLog` so the services can
//!   be exercised without Postgres.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use kevin_domain::task::{RouteSelectionInfo, RoutingPolicy};
use kevin_domain::{
    ArtifactRef, AttemptId, Complexity, Effort, FailureClass, Goal, ModelAlias, Plan, Route, RunId,
    RunMode, TaskId, TaskKind, Understanding, Usage, Workspace, WorkspacePolicy,
};
use kevin_domain::{CommandId, run::RunEvaluation};

use crate::roles::SystemContextSection;
use rust_decimal::Decimal;
use serde_json::Value;

/// Failure of any port. Ports never carry business rules, so one flat error
/// with a retryability hint is enough for the saga's classification table.
#[derive(Debug, thiserror::Error)]
pub struct PortError {
    /// Which port failed (`router`, `roles`, `memory`, `evaluator`, `workspace`).
    pub port: &'static str,
    /// Human-readable cause.
    pub message: String,
    /// How the saga must classify it.
    pub class: FailureClass,
}

impl PortError {
    /// A transient port failure (retryable).
    pub fn transient(port: &'static str, message: impl Into<String>) -> Self {
        Self {
            port,
            message: message.into(),
            class: FailureClass::Transient,
        }
    }

    /// A permanent port failure (not retryable).
    pub fn permanent(port: &'static str, message: impl Into<String>) -> Self {
        Self {
            port,
            message: message.into(),
            class: FailureClass::Permanent,
        }
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} port: {} ({})", self.port, self.message, self.class)
    }
}

/// Convenience alias for port results.
pub type PortResult<T> = Result<T, PortError>;

// ---------------------------------------------------------------------------
// Router (WS-09)
// ---------------------------------------------------------------------------

/// What the saga asks the router for (`plan/06` §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectRouteQuery {
    /// Task kind being routed.
    pub kind: TaskKind,
    /// Complexity from the understanding.
    pub complexity: Complexity,
    /// Free-form tags (plan hints).
    pub tags: Vec<String>,
    /// Aliases that already failed on this task.
    pub exclude: Vec<ModelAlias>,
    /// Remaining run budget, when known.
    pub budget_left_usd: Option<Decimal>,
    /// Seeded RNG for reproducible tests.
    pub rng_seed: Option<u64>,
}

impl SelectRouteQuery {
    /// A query for `kind` at `complexity` with no exclusions.
    #[must_use]
    pub fn new(kind: TaskKind, complexity: Complexity) -> Self {
        Self {
            kind,
            complexity,
            tags: Vec::new(),
            exclude: Vec::new(),
            budget_left_usd: None,
            rng_seed: None,
        }
    }

    /// Excludes the aliases that already failed.
    #[must_use]
    pub fn excluding(mut self, aliases: Vec<ModelAlias>) -> Self {
        self.exclude = aliases;
        self
    }
}

/// One candidate the router scored (`plan/06` §2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateScore {
    /// The alias.
    pub alias: ModelAlias,
    /// Sampled success probability.
    pub sampled_success: f32,
    /// Quality prior / EMA.
    pub quality: f32,
    /// Min-max normalised cost.
    pub norm_cost: f32,
    /// Min-max normalised latency.
    pub norm_latency: f32,
    /// Final score.
    pub score: f32,
    /// Observations behind the score.
    pub samples: u32,
    /// Why the candidate was dropped, when it was.
    pub excluded_reason: Option<String>,
}

/// What the router answered (`plan/06` §2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteSelection {
    /// The chosen route.
    pub route: Route,
    /// Policy that produced it.
    pub policy: RoutingPolicy,
    /// Every candidate considered.
    pub candidates: Vec<CandidateScore>,
    /// Catalog version the scores refer to.
    pub catalog_version: String,
}

impl RouteSelection {
    /// A deterministic single-candidate selection (`policy = fixed`).
    #[must_use]
    pub fn fixed(route: Route) -> Self {
        let alias = route.model.clone();
        Self {
            route,
            policy: RoutingPolicy::Fixed,
            candidates: vec![CandidateScore {
                alias,
                sampled_success: 1.0,
                quality: 1.0,
                norm_cost: 0.0,
                norm_latency: 0.0,
                score: 1.0,
                samples: 0,
                excluded_reason: None,
            }],
            catalog_version: String::new(),
        }
    }

    /// The `task.routed.selection` payload for this selection.
    #[must_use]
    pub fn selection_info(&self) -> RouteSelectionInfo {
        RouteSelectionInfo {
            policy: self.policy,
            candidates: self.candidates.iter().map(|c| c.alias.clone()).collect(),
            scores: self.candidates.iter().map(|c| c.score).collect(),
            catalog_version: (!self.catalog_version.is_empty())
                .then(|| self.catalog_version.clone()),
        }
    }
}

/// What the saga reports back after a terminal attempt (`plan/06` §2.4).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordRouteOutcome {
    /// The run.
    pub run_id: RunId,
    /// The task.
    pub task_id: TaskId,
    /// The attempt (idempotency key of `routing.route_outcomes`).
    pub attempt_id: AttemptId,
    /// Task kind.
    pub task_kind: TaskKind,
    /// Alias used.
    pub alias: ModelAlias,
    /// Whether the attempt succeeded.
    pub success: bool,
    /// Judge score when one exists.
    pub quality: Option<f32>,
    /// Cost of the attempt.
    pub cost_usd: Option<Decimal>,
    /// Wall-clock of the attempt.
    pub wall_ms: u64,
    /// Failure class when it failed.
    pub failure_class: Option<FailureClass>,
}

/// Model selection and outcome recording (`kevin-router`, WS-09).
#[async_trait]
pub trait RouterPort: Send + Sync {
    /// Chooses a route for one task attempt.
    async fn select(&self, query: SelectRouteQuery) -> PortResult<RouteSelection>;

    /// Records how the attempt went, so the next selection learns from it.
    async fn record_outcome(&self, outcome: RecordRouteOutcome) -> PortResult<()>;
}

// ---------------------------------------------------------------------------
// Roles (WS-10)
// ---------------------------------------------------------------------------

/// An answered clarification question, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsweredQuestion {
    /// The question text.
    pub question: String,
    /// The answer, rendered as text.
    pub answer: String,
    /// Who answered (`default` for applied defaults).
    pub answered_by: String,
}

/// Everything a role call needs about the run (`plan/05` §3.1–§3.4).
#[derive(Debug, Clone)]
pub struct RoleContext {
    /// The run.
    pub run_id: RunId,
    /// The goal.
    pub goal: Goal,
    /// Interaction mode (drives the "never wait" rules).
    pub mode: RunMode,
    /// Rendered `<kevin-memory>` block.
    pub memory: Option<String>,
    /// Platform briefings from
    /// [`SystemContextProvider`](crate::roles::SystemContextProvider)s.
    pub system_context: Vec<SystemContextSection>,
    /// The recorded understanding (planning calls only).
    pub understanding: Option<Understanding>,
    /// Answers to the clarification questions.
    pub answers: Vec<AnsweredQuestion>,
    /// The previously proposed plan when re-planning.
    pub previous_plan: Option<Plan>,
    /// Reviewer feedback from `run.plan_rejected`.
    pub feedback: Option<String>,
    /// Validation errors of the previous plan (repair call).
    pub repair_errors: Vec<String>,
    /// Route the role runs on (`[roles]`).
    pub route: Route,
    /// Effort for the role (`roles.effort.<role>`).
    pub effort: Option<Effort>,
}

impl RoleContext {
    /// A context with just the goal and the role's route.
    #[must_use]
    pub fn new(run_id: RunId, goal: Goal, mode: RunMode, route: Route) -> Self {
        Self {
            run_id,
            goal,
            mode,
            memory: None,
            system_context: Vec::new(),
            understanding: None,
            answers: Vec::new(),
            previous_plan: None,
            feedback: None,
            repair_errors: Vec::new(),
            route,
            effort: None,
        }
    }

    /// `true` when this is the repair call after an invalid plan.
    #[must_use]
    pub fn is_repair(&self) -> bool {
        !self.repair_errors.is_empty()
    }
}

/// What the integrator role is asked to summarise (`plan/05` §3.6).
#[derive(Debug, Clone)]
pub struct IntegrateContext {
    /// The run.
    pub run_id: RunId,
    /// The goal.
    pub goal: Goal,
    /// Acceptance criteria of the approved plan.
    pub acceptance_criteria: Vec<String>,
    /// Per-task summaries of the succeeded tasks.
    pub task_summaries: Vec<String>,
    /// Artifacts the integration produced.
    pub artifacts: Vec<ArtifactRef>,
    /// Unresolved conflicts, when the integration was not clean.
    pub conflicts: Vec<String>,
    /// Route the role runs on.
    pub route: Route,
}

/// What the integrator role answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationSummary {
    /// Human-readable summary recorded on `run.integrated` / `run.completed`.
    pub summary: String,
}

/// Kevin's own roles: planner (understanding + plan) and integrator
/// (`kevin-orchestrator::roles`, WS-10 — its `RoleRunner` implements this).
#[async_trait]
pub trait RolesPort: Send + Sync {
    /// Runs the `understanding` call of the planner role.
    async fn understanding(&self, ctx: &RoleContext) -> PortResult<(Understanding, Usage)>;

    /// Runs the `plan` call of the planner role.
    async fn plan(&self, ctx: &RoleContext) -> PortResult<(Plan, Usage)>;

    /// Runs the integrator role to summarise what was integrated.
    async fn integrate(&self, ctx: &IntegrateContext) -> PortResult<(IntegrationSummary, Usage)>;
}

// ---------------------------------------------------------------------------
// Memory (WS-18)
// ---------------------------------------------------------------------------

/// A lesson the saga wants remembered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lesson {
    /// The run it came from.
    pub run_id: RunId,
    /// Lesson body.
    pub content: String,
    /// Tags (`repo:<name>`, task kind, …).
    pub tags: Vec<String>,
}

/// Retrieval and storage of long-term memory (`kevin-memory`, WS-18).
#[async_trait]
pub trait MemoryPort: Send + Sync {
    /// The `<kevin-memory>` block injected into the intake call, already
    /// capped at `memory.context_max_tokens`. `None` = nothing relevant.
    async fn context_for_intake(
        &self,
        goal: &Goal,
        repo: Option<&str>,
    ) -> PortResult<Option<String>>;

    /// Stores one lesson learned by a run.
    async fn store_lesson(&self, lesson: Lesson) -> PortResult<()>;
}

// ---------------------------------------------------------------------------
// Evaluator (WS-19)
// ---------------------------------------------------------------------------

/// The judge (`kevin-evaluator`, WS-19).
#[async_trait]
pub trait EvaluatorPort: Send + Sync {
    /// Evaluates a finished run and its tasks. `Ok(None)` means "evaluated but
    /// no run-level verdict"; the saga completes the run either way, and a
    /// call that outlives `orchestrator.evaluation_timeout` is abandoned and
    /// the run completes with `evaluation: skipped` (`plan/05` §3.7).
    async fn evaluate_run(
        &self,
        run_id: RunId,
        task_ids: &[TaskId],
    ) -> PortResult<Option<RunEvaluation>>;
}

// ---------------------------------------------------------------------------
// Workspace (kevin-workspace, WS-07 — async seam)
// ---------------------------------------------------------------------------

/// What the scheduler asks for before an attempt starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareWorkspace {
    /// The run.
    pub run_id: RunId,
    /// The task.
    pub task_id: TaskId,
    /// The attempt (a retry always gets a fresh workspace).
    pub attempt_id: AttemptId,
    /// Task title, slugified into paths and branch names.
    pub task_slug: String,
    /// Plan-level policy.
    pub policy: WorkspacePolicy,
}

/// What the saga asks the integrator for.
#[derive(Debug, Clone)]
pub struct IntegrateRequest {
    /// The run.
    pub run_id: RunId,
    /// PR title / merge subject.
    pub title: String,
    /// Summary for the PR body.
    pub summary: String,
    /// Acceptance criteria of the **approved plan**.
    pub acceptance_criteria: Vec<String>,
    /// Workspaces of the succeeded tasks, in plan order.
    pub workspaces: Vec<Workspace>,
}

/// What the integrator answered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IntegrationOutcome {
    /// PR URLs, diffs, branch names.
    pub artifacts: Vec<ArtifactRef>,
    /// Conflicting sources, empty when clean.
    pub conflicts: Vec<String>,
}

impl IntegrationOutcome {
    /// `true` when nothing conflicted.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// Per-attempt workspace isolation and result integration (`kevin-workspace`).
#[async_trait]
pub trait WorkspacePort: Send + Sync {
    /// Prepares the checkout the attempt runs in.
    async fn prepare(&self, req: PrepareWorkspace) -> PortResult<Workspace>;

    /// Applies `workspace.cleanup` to an attempt workspace.
    async fn cleanup(&self, workspace: &Workspace, succeeded: bool) -> PortResult<()>;

    /// Merges the succeeded tasks per `workspace.integration`.
    async fn integrate(&self, req: IntegrateRequest) -> PortResult<IntegrationOutcome>;
}

// ---------------------------------------------------------------------------
// Command idempotency (kevin-store::CommandLog)
// ---------------------------------------------------------------------------

/// Idempotent command replay over `core.processed_commands`.
///
/// [`CommandIdempotency::begin`] returns the recorded result when the command
/// already ran; [`CommandIdempotency::complete`] records it (and returns the
/// winner's result when a concurrent execution recorded first).
#[async_trait]
pub trait CommandIdempotency: Send + Sync {
    /// The recorded result of `command_id`, if any.
    async fn begin(&self, command_id: CommandId) -> Result<Option<Value>, kevin_store::StoreError>;

    /// Records `result` unless a result already exists; returns the winner.
    async fn complete(
        &self,
        command_id: CommandId,
        result: &Value,
    ) -> Result<Value, kevin_store::StoreError>;
}

#[async_trait]
impl CommandIdempotency for kevin_store::CommandLog {
    async fn begin(&self, command_id: CommandId) -> Result<Option<Value>, kevin_store::StoreError> {
        self.result_of(command_id).await
    }

    async fn complete(
        &self,
        command_id: CommandId,
        result: &Value,
    ) -> Result<Value, kevin_store::StoreError> {
        match kevin_store::CommandLog::complete(self, command_id, result).await? {
            kevin_store::CompleteOutcome::Recorded => Ok(result.clone()),
            kevin_store::CompleteOutcome::AlreadyRecorded(existing) => Ok(existing),
        }
    }
}

#[async_trait]
impl<P: CommandIdempotency + ?Sized> CommandIdempotency for Arc<P> {
    async fn begin(&self, command_id: CommandId) -> Result<Option<Value>, kevin_store::StoreError> {
        (**self).begin(command_id).await
    }

    async fn complete(
        &self,
        command_id: CommandId,
        result: &Value,
    ) -> Result<Value, kevin_store::StoreError> {
        (**self).complete(command_id, result).await
    }
}
