//! Wire types of the HTTP API (`plan/07-api-and-tui.md` §DTOs).
//!
//! Every type here is `serde` (JSON) and, under the `server` feature, also
//! [`utoipa::ToSchema`] so the same definitions produce
//! `GET /api/v1/openapi.json`. The DTOs are deliberately *flat mirrors* of the
//! read models: the API never leaks a domain aggregate, and a projection
//! column change cannot silently change the wire format.
//!
//! Conventions (plan/07 §Conventions): timestamps are RFC 3339 UTC, ids are
//! uuid strings, money is a **decimal string** (`"0.0421"`) and never a float.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use kevin_domain::ids::{
    ArtifactId, AttemptId, EvaluationId, EventId, MemoryItemId, ProposalId, QuestionId, RunId,
    TaskId,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Derives `utoipa::ToSchema` only when the server feature is on (the client
/// build must not pull axum/utoipa in).
macro_rules! schema {
    ($($item:item)*) => {
        $(
            #[cfg_attr(feature = "server", derive(utoipa::ToSchema))]
            $item
        )*
    };
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// One page of `T` plus the cursor that fetches the next one
/// (`?cursor=<opaque>&limit=50`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "server", derive(utoipa::ToSchema))]
pub struct Page<T> {
    /// The rows of this page.
    pub items: Vec<T>,
    /// Opaque cursor for the next page; `None` when this was the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> Page<T> {
    /// A page with no cursor.
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            next_cursor: None,
        }
    }

    /// An empty last page.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_cursor: None,
        }
    }

    /// Maps the items, keeping the cursor.
    pub fn map<U>(self, f: impl FnMut(T) -> U) -> Page<U> {
        Page {
            items: self.items.into_iter().map(f).collect(),
            next_cursor: self.next_cursor,
        }
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Shared value objects
// ---------------------------------------------------------------------------

schema! {
    /// Token/cost counters rolled up over a run, task or attempt.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct UsageDto {
        /// Prompt tokens.
        pub input_tokens: u64,
        /// Completion tokens.
        pub output_tokens: u64,
        /// Cache reads.
        #[serde(default)]
        pub cache_read_tokens: u64,
        /// Cache writes.
        #[serde(default)]
        pub cache_write_tokens: u64,
        /// Spend, as a decimal string; `None` when no price is known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub cost_usd: Option<Decimal>,
        /// Wall-clock milliseconds.
        #[serde(default)]
        pub wall_ms: u64,
    }

    /// Caps applied to a run or a task.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct BudgetDto {
        /// USD cap, as a decimal string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub max_usd: Option<Decimal>,
        /// Token cap (input + output).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_tokens: Option<u64>,
        /// Wall-clock cap in milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_wall_ms: Option<u64>,
        /// Attempts per task.
        #[serde(default)]
        pub max_attempts: u8,
        /// Tasks in flight.
        #[serde(default)]
        pub max_parallel: u16,
    }

    /// The `(worker, model, effort)` triple a task was routed to.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RouteDto {
        /// Worker CLI (`claude`, `codex`, `pi`, `opencode`, `fake`).
        pub worker: String,
        /// Model alias from the catalogue.
        pub model: String,
        /// Reasoning effort, when the worker supports one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub effort: Option<String>,
    }

    /// Where an attempt ran.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WorkspaceDto {
        /// Absolute path of the workspace root.
        #[cfg_attr(feature = "server", schema(value_type = String))]
        pub root: PathBuf,
        /// `git_worktree` | `jj_workspace` | `in_place`.
        pub kind: String,
        /// Base revision the workspace was created from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub base_rev: Option<String>,
    }

    /// Why an attempt failed.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FailureDto {
        /// `transient` | `permanent` | `timeout` | `killed` | `policy_violation` | …
        pub class: String,
        /// Human-readable, redacted message.
        pub message: String,
    }

    /// A blob produced by a task or by integration.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ArtifactDto {
        /// Artifact id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: ArtifactId,
        /// Owning run.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub run_id: RunId,
        /// Producing task, when any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>, format = Uuid))]
        pub task_id: Option<TaskId>,
        /// `diff` | `file` | `pr_url` | `report` | `json` | `transcript`.
        pub kind: String,
        /// Where the bytes live (`file:///…`, `https://…`).
        pub uri: String,
        /// Hex SHA-256, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sha256: Option<String>,
        /// Size in bytes, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub bytes: Option<u64>,
        /// `task` | `run`.
        pub produced_by: String,
        /// When it was produced.
        pub created_at: DateTime<Utc>,
    }

    /// A file or URL attached to the goal. Attachments are passed **by
    /// reference**; the API never accepts inline bytes (plan/07 §Limits).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AttachmentRef {
        /// `diff` | `file` | `pr_url` | `report` | `json` | `transcript`.
        #[serde(default = "default_attachment_kind")]
        pub kind: String,
        /// Where the bytes live.
        pub uri: String,
        /// Size in bytes, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub bytes: Option<u64>,
    }
}

fn default_attachment_kind() -> String {
    "file".to_owned()
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

schema! {
    /// Lifecycle of a run; identical to the domain `RunStatus` serde form.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RunStatusDto {
        /// `run.started` applied.
        Received,
        /// Planner is producing the understanding.
        Understanding,
        /// Clarification questions are open.
        AwaitingAnswers,
        /// Planner is producing (or revising) the plan.
        Planning,
        /// Waiting for a human to approve the plan.
        AwaitingPlanApproval,
        /// Tasks are scheduled and running.
        Executing,
        /// Integrator is merging / opening PRs.
        Integrating,
        /// Judge is evaluating the integrated result.
        Evaluating,
        /// Terminal: success.
        Completed,
        /// Terminal: failure.
        Failed,
        /// Terminal: cancelled.
        Cancelled,
    }

    /// How a run interacts with humans.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RunModeDto {
        /// Questions block, plans need approval.
        #[default]
        Interactive,
        /// Questions default/expire, plans auto-approved.
        Headless,
        /// One Kohral turn; never waits for a human.
        Kohral,
    }

    /// What the operator asked for.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct GoalDto {
        /// The goal text (≤ 64 KiB).
        pub text: String,
        /// Attachments, by reference.
        #[serde(default)]
        pub attachments: Vec<AttachmentRef>,
        /// Working directory the run targets.
        #[cfg_attr(feature = "server", schema(value_type = String))]
        pub cwd: PathBuf,
        /// `git` | `jj` | `plain`.
        pub repo_kind: String,
    }

    /// How many tasks are in each terminal state.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TaskCountsDto {
        /// Tasks in the plan.
        pub total: u32,
        /// Succeeded.
        pub succeeded: u32,
        /// Failed.
        pub failed: u32,
        /// Cancelled.
        pub cancelled: u32,
        /// Skipped (a dependency failed).
        pub skipped: u32,
    }

    /// The judge's verdict on a run.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EvaluationSummaryDto {
        /// The evaluation aggregate.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: EvaluationId,
        /// Overall score in `0.0..=1.0`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub overall: Option<f32>,
        /// `accept` | `revise` | `reject`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub verdict: Option<String>,
    }

    /// A task as it appears inside [`RunDto`].
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct TaskSummaryDto {
        /// Task id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: TaskId,
        /// Task kind (`implement`, `test`, …).
        pub kind: String,
        /// One-line title from the plan.
        pub title: String,
        /// Current status.
        pub status: String,
        /// Route of the current/last attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub route: Option<RouteDto>,
        /// Attempts made so far.
        pub attempt_count: u32,
        /// Rolled-up usage.
        pub usage: UsageDto,
    }

    /// `POST /api/v1/runs`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CreateRunRequest {
        /// What to do (≤ 64 KiB).
        pub goal: String,
        /// Working directory; defaults to the server's cwd.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub cwd: Option<PathBuf>,
        /// Attachments, by reference.
        #[serde(default)]
        pub attachments: Vec<AttachmentRef>,
        /// `interactive` (default) or `headless`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mode: Option<RunModeDto>,
        /// Caps; unset dimensions fall back to `[budget]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub budget: Option<BudgetDto>,
        /// Free-form tags.
        #[serde(default)]
        pub tags: Vec<String>,
    }

    /// A run in full (`GET /api/v1/runs/{run_id}`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunDto {
        /// Run id (also the `correlation_id` of every event it causes).
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: RunId,
        /// Current status.
        pub status: RunStatusDto,
        /// What was asked.
        pub goal: GoalDto,
        /// Interaction mode.
        pub mode: RunModeDto,
        /// Effective caps.
        pub budget: BudgetDto,
        /// Rolled-up usage.
        pub usage: UsageDto,
        /// Planner output, once available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub understanding: Option<UnderstandingDto>,
        /// Task graph, once proposed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub plan: Option<PlanDto>,
        /// Questions still waiting for an answer.
        #[serde(default)]
        #[cfg_attr(feature = "server", schema(value_type = Vec<String>))]
        pub open_questions: Vec<QuestionId>,
        /// Tasks of the plan, in plan order.
        #[serde(default)]
        pub tasks: Vec<TaskSummaryDto>,
        /// Judge verdict, once evaluated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub evaluation: Option<EvaluationSummaryDto>,
        /// When the run was started.
        pub created_at: DateTime<Utc>,
        /// Last applied event time.
        pub updated_at: DateTime<Utc>,
        /// Aggregate version this view reflects.
        pub version: u64,
    }

    /// A run in a list (`GET /api/v1/runs`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunSummaryDto {
        /// Run id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: RunId,
        /// Current status.
        pub status: RunStatusDto,
        /// First line of the goal, truncated.
        pub goal_excerpt: String,
        /// Rolled-up usage.
        pub usage: UsageDto,
        /// Task tallies.
        pub task_counts: TaskCountsDto,
        /// When the run was started.
        pub created_at: DateTime<Utc>,
        /// Last applied event time.
        pub updated_at: DateTime<Utc>,
    }

    /// `POST /api/v1/runs/{run_id}/cancel`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CancelRunRequest {
        /// Why the run is being cancelled (recorded on the event).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }

    /// A command endpoint whose body is `{}` (present so the OpenAPI document
    /// documents the shape instead of an opaque byte stream).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct EmptyRequest {}

    /// `POST /api/v1/runs/{run_id}/plan/reject`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RejectPlanRequest {
        /// What the planner should change (required).
        pub feedback: String,
    }
}

/// The planner's understanding of the goal (the domain `Understanding` JSON,
/// `plan/05-orchestration.md` §Schemas). Kept opaque on the wire so a change
/// to the planner's schema is not a breaking API change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnderstandingDto(pub Value);

/// The proposed or approved task graph (the domain `Plan` JSON,
/// `plan/05-orchestration.md` §Schemas).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanDto(pub Value);

#[cfg(feature = "server")]
mod opaque_schemas {
    use utoipa::openapi::{ObjectBuilder, RefOr, Schema};
    use utoipa::{PartialSchema, ToSchema};

    macro_rules! opaque {
        ($($ty:ty),+ $(,)?) => {$(
            impl PartialSchema for $ty {
                fn schema() -> RefOr<Schema> {
                    ObjectBuilder::new().into()
                }
            }
            impl ToSchema for $ty {}
        )+};
    }

    opaque!(super::UnderstandingDto, super::PlanDto);
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

schema! {
    /// One worker run of a task.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AttemptDto {
        /// Attempt id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: AttemptId,
        /// 1-based attempt number within the run.
        pub no: u8,
        /// Route this attempt used.
        pub route: RouteDto,
        /// `running` | `succeeded` | `failed` | `cancelled`.
        pub status: String,
        /// Isolated workspace, when one was created.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub workspace: Option<WorkspaceDto>,
        /// Resumable worker session, when the CLI exposes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub worker_session_id: Option<String>,
        /// When the worker was spawned.
        pub started_at: DateTime<Utc>,
        /// When the attempt reached a terminal state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ended_at: Option<DateTime<Utc>>,
        /// Usage of this attempt.
        pub usage: UsageDto,
        /// Failure, when it failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub failure: Option<FailureDto>,
    }

    /// A task in full (`GET /api/v1/tasks/{task_id}`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct TaskDto {
        /// Task id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: TaskId,
        /// Owning run.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub run_id: RunId,
        /// Task kind (`implement`, `test`, … or a custom name).
        pub kind: String,
        /// One-line title from the plan.
        pub title: String,
        /// Current status.
        pub status: String,
        /// Route of the current/last attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub route: Option<RouteDto>,
        /// Attempts, oldest first.
        #[serde(default)]
        pub attempts: Vec<AttemptDto>,
        /// Tasks that must finish first.
        #[serde(default)]
        #[cfg_attr(feature = "server", schema(value_type = Vec<String>))]
        pub depends_on: Vec<TaskId>,
        /// Rolled-up usage.
        pub usage: UsageDto,
        /// Artifacts produced by the task.
        #[serde(default)]
        pub artifacts: Vec<ArtifactDto>,
        /// What "done" means for this task.
        #[serde(default)]
        pub acceptance_criteria: Vec<String>,
    }

    /// One transcript line (`GET /api/v1/tasks/{task_id}/log`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct TaskLogLineDto {
        /// Strictly increasing within `(task_id, attempt)`; the SSE `Last-Event-ID`.
        pub seq: u64,
        /// Attempt number (`0` = task-level).
        pub attempt: u8,
        /// When the line was produced.
        pub at: DateTime<Utc>,
        /// `assistant` | `tool_call` | `tool_result` | `usage` | `system`.
        pub kind: String,
        /// The redacted line body.
        #[cfg_attr(feature = "server", schema(value_type = Object))]
        pub payload: Value,
    }

    /// `POST /api/v1/tasks/{task_id}/retry`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RetryTaskRequest {
        /// Ask the router not to pick the failing route again.
        #[serde(default)]
        pub exclude_route: bool,
    }
}

// ---------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------

schema! {
    /// One selectable option of a question.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QuestionOptionDto {
        /// Short label (what the client selects).
        pub label: String,
        /// Longer explanation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        /// The planner's recommendation.
        #[serde(default)]
        pub recommended: bool,
    }

    /// An answer, as stored.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AnswerDto {
        /// Selected option labels.
        #[serde(default)]
        pub selected: Vec<String>,
        /// Free text when the options do not fit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub free_text: Option<String>,
        /// Who answered (`<user>`, `default`, `kohral`).
        pub answered_by: String,
    }

    /// When an unanswered question stops blocking.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QuestionPolicyDto {
        /// `block` | `default_after`.
        pub kind: QuestionPolicyKind,
        /// Timeout in milliseconds for `default_after`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timeout_ms: Option<u64>,
    }

    /// Discriminant of [`QuestionPolicyDto`].
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum QuestionPolicyKind {
        /// Wait for a human indefinitely.
        #[default]
        Block,
        /// Apply the default after the timeout.
        DefaultAfter,
    }

    /// A clarification question.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QuestionDto {
        /// Question id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: QuestionId,
        /// Owning run.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub run_id: RunId,
        /// Asking task, when a task asked it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>, format = Uuid))]
        pub task_id: Option<TaskId>,
        /// The question.
        pub text: String,
        /// Selectable options.
        #[serde(default)]
        pub options: Vec<QuestionOptionDto>,
        /// Whether several options may be selected.
        pub multi_select: bool,
        /// Applied when the policy expires.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default: Option<AnswerDto>,
        /// Blocking behaviour.
        pub policy: QuestionPolicyDto,
        /// `open` | `answered` | `expired`.
        pub status: String,
        /// The answer, once given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub answer: Option<AnswerDto>,
        /// When it was asked.
        pub asked_at: DateTime<Utc>,
    }

    /// `POST /api/v1/questions/{question_id}/answer`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct AnswerRequest {
        /// Selected option labels.
        #[serde(default)]
        pub selected: Vec<String>,
        /// Free text when the options do not fit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub free_text: Option<String>,
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

schema! {
    /// One persisted domain event, as delivered over SSE and by catch-up.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EventDto {
        /// Global position; the SSE `id:` and `Last-Event-ID`.
        pub position: u64,
        /// Event id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub event_id: EventId,
        /// Stable event name (`run.started`, `task.attempt_failed`, …).
        pub event_type: String,
        /// When it happened.
        pub occurred_at: DateTime<Utc>,
        /// `run` | `task` | `question` | `evaluation` | `route_score` | `memory_item`.
        pub aggregate_type: String,
        /// Id of the aggregate.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub aggregate_id: Uuid,
        /// Version of the aggregate after this event.
        pub aggregate_version: u64,
        /// Always the run id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub correlation_id: Uuid,
        /// The redacted payload.
        #[cfg_attr(feature = "server", schema(value_type = Object))]
        pub payload: Value,
    }
}

/// SSE `event:` name emitted when the live bus dropped events and the client
/// must refetch a snapshot (plan/07 §Event streams).
pub const SSE_RESYNC: &str = "resync";

/// SSE `event:` name of the synthetic snapshot emitted on a live-only connect.
pub const SSE_SNAPSHOT: &str = "snapshot";

schema! {
    /// Body of the `resync` SSE event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ResyncDto {
        /// First position that was dropped.
        pub from: u64,
        /// Last position that was dropped.
        pub to: u64,
    }
}

// ---------------------------------------------------------------------------
// Cost, routes, memory, proposals
// ---------------------------------------------------------------------------

schema! {
    /// One grouped row of [`CostReportDto`].
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CostRowDto {
        /// Run id, model alias or task kind, depending on `group_by`.
        pub key: String,
        /// Spend, as a decimal string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub usd: Option<Decimal>,
        /// Prompt tokens.
        pub input_tokens: u64,
        /// Completion tokens.
        pub output_tokens: u64,
        /// Attempts counted.
        pub attempts: u32,
    }

    /// `GET /api/v1/cost`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CostReportDto {
        /// Total spend, as a decimal string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub total_usd: Option<Decimal>,
        /// Total tokens (input + output).
        pub total_tokens: u64,
        /// The grouped rows, biggest spender first.
        pub rows: Vec<CostRowDto>,
    }

    /// One row of the routing leaderboard (`GET /api/v1/routes`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RouteScoreDto {
        /// Task kind the score is for.
        pub kind: String,
        /// Model alias.
        pub alias: String,
        /// Attempts observed.
        pub attempts: u32,
        /// Attempts that succeeded.
        pub successes: u32,
        /// Mean judge score in `0.0..=1.0`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mean_quality: Option<f32>,
        /// Mean spend per attempt, as a decimal string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub mean_cost_usd: Option<Decimal>,
        /// Mean wall-clock milliseconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mean_wall_ms: Option<u64>,
        /// Last Thompson sample, when the router exposes one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sampled_score: Option<f32>,
    }

    /// A memory item (`GET /api/v1/memory/search`, `GET /api/v1/lessons`).
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct MemoryItemDto {
        /// Item id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: MemoryItemId,
        /// `fact` | `preference` | `lesson` | `pattern` | `pitfall` | `summary`.
        pub kind: String,
        /// The stored (redacted) content.
        pub content: String,
        /// Free-form tags.
        #[serde(default)]
        pub tags: Vec<String>,
        /// Operator-assigned importance in `0.0..=1.0`.
        pub importance: f32,
        /// Hybrid similarity of this hit; `None` outside a search.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub similarity: Option<f32>,
        /// Provenance (`run_id`, `task_id`, `actor`, …).
        #[cfg_attr(feature = "server", schema(value_type = Object))]
        pub source: Value,
        /// When it was stored.
        pub created_at: DateTime<Utc>,
    }

    /// A change an evaluation proposes (`GET /api/v1/proposals`).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ProposalDto {
        /// Proposal id.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub id: ProposalId,
        /// The evaluation that raised it.
        #[cfg_attr(feature = "server", schema(value_type = String, format = Uuid))]
        pub evaluation_id: EvaluationId,
        /// `prompt` | `config` | `routing`.
        pub kind: String,
        /// The proposed change.
        pub body: String,
        /// `proposed` | `accepted` | `rejected`.
        pub status: String,
        /// When it was raised.
        pub created_at: DateTime<Utc>,
    }

    /// `POST /api/v1/proposals/{id}/accept|reject`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ProposalDecisionRequest {
        /// Operator note recorded with the decision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub note: Option<String>,
    }
}

// ---------------------------------------------------------------------------
// Workers, config, health, drain
// ---------------------------------------------------------------------------

schema! {
    /// One row of `kevin workers doctor` (`GET /api/v1/workers`).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WorkerDoctorDto {
        /// `claude` | `codex` | `pi` | `opencode` | `fake`.
        pub kind: String,
        /// Whether the worker is enabled in the configuration.
        pub enabled: bool,
        /// Resolved binary path, `None` when missing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>))]
        pub binary: Option<PathBuf>,
        /// Version string reported by the binary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub version: Option<String>,
        /// `Some(true|false)` when auth could be probed, `None` when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub auth_ready: Option<bool>,
        /// Everything that would stop this worker from running.
        #[serde(default)]
        pub problems: Vec<String>,
    }

    /// `GET|POST|DELETE /api/v1/maintenance/drain`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DrainStatusDto {
        /// Whether new runs are refused.
        pub draining: bool,
        /// Runs that have not reached a terminal state yet.
        pub running_runs: u32,
        /// Worker attempts still in flight.
        pub running_attempts: u32,
    }

    /// `GET /healthz`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct HealthDto {
        /// Always `"ok"` when the process answers at all.
        pub status: String,
    }

    /// `GET /readyz`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ReadyDto {
        /// `true` when the API accepts new runs.
        pub ready: bool,
        /// The database answered a ping within the deadline.
        pub db: bool,
        /// Admission is closed.
        pub draining: bool,
        /// Every enabled worker passed `doctor`.
        pub workers_ok: bool,
    }

    /// `GET /api/v1/config` — the effective configuration with secrets redacted.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ConfigDto {
        /// The redacted configuration tree.
        #[cfg_attr(feature = "server", schema(value_type = Object))]
        pub config: Value,
        /// Where each leaf key came from (`default`, `user`, `env`, …).
        pub sources: BTreeMap<String, String>,
    }
}

// ---------------------------------------------------------------------------
// Query parameters used by both the router and the typed client
// ---------------------------------------------------------------------------

schema! {
    /// `GET /api/v1/runs`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ListRunsQuery {
        /// Keep only runs in this status.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
        /// Cursor from a previous [`Page::next_cursor`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cursor: Option<String>,
        /// Page size (max 200).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<usize>,
    }

    /// `GET /api/v1/questions`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QuestionsQuery {
        /// Keep only questions of this run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>, format = Uuid))]
        pub run_id: Option<RunId>,
        /// Keep only this status (`open` for the inbox).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
        /// Cursor from a previous page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cursor: Option<String>,
        /// Page size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<usize>,
    }

    /// `GET /api/v1/tasks/{task_id}/log`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TaskLogQueryDto {
        /// Keep only this attempt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub attempt: Option<u8>,
        /// Return lines with `seq > after_seq`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub after_seq: Option<u64>,
        /// Page size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<usize>,
    }

    /// `GET /api/v1/cost`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CostQueryDto {
        /// Only attempts started at or after this instant.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub since: Option<DateTime<Utc>>,
        /// Only this run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "server", schema(value_type = Option<String>, format = Uuid))]
        pub run_id: Option<RunId>,
        /// `run` (default) | `model` | `kind`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub group_by: Option<String>,
    }

    /// `GET /api/v1/memory/search`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct MemorySearchQuery {
        /// The query text.
        pub q: String,
        /// Comma-separated memory kinds to keep.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub kinds: Option<String>,
        /// How many hits to return.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub top_k: Option<usize>,
    }

    /// `GET /api/v1/lessons`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LessonsQuery {
        /// Cursor from a previous page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cursor: Option<String>,
        /// Page size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<usize>,
    }

    /// `GET /api/v1/proposals`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ProposalsQuery {
        /// Keep only this status (`proposed` for the inbox).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
        /// Cursor from a previous page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cursor: Option<String>,
        /// Page size.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<usize>,
    }

    /// `GET /api/v1/routes`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RoutesQuery {
        /// Keep only this task kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub kind: Option<String>,
    }

    /// `GET /api/v1/events` and `GET /api/v1/runs/{run_id}/events`.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct EventStreamQuery {
        /// Comma-separated event-type prefixes (`run.*,task.*`) or exact names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub types: Option<String>,
        /// Replay from this position (`0` = from the beginning). Ignored when
        /// `Last-Event-ID` is present.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub from: Option<u64>,
    }
}
