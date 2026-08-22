//! Read model rows → wire DTOs (`server` feature only).
//!
//! The projections store the domain value objects as JSONB, so most of the
//! work here is "decode the JSON the projection wrote, then flatten it into the
//! DTO". Anything that fails to decode is treated as absent rather than as an
//! error: a read endpoint must not 500 because one optional column of one row
//! predates an upcaster.

use chrono::{DateTime, Utc};
use kevin_domain::Aggregate;
use kevin_domain::ids::{
    ArtifactId, AttemptId, EvaluationId, EventId, MemoryItemId, QuestionId, RunId, TaskId,
};
use kevin_domain::values::{
    Answer, Budget, QuestionOption, QuestionPolicy, Route, Usage, Workspace,
};
use kevin_memory::store::{Hit, Lesson};
use kevin_orchestrator::projections::{
    ArtifactRow, CostReport, QuestionInboxRow, RunOverviewRow, TaskBoardRow, TaskLogRow,
};
use kevin_store::StoredEvent;
use kevin_worker::worker::{AuthStatus, Doctor};
use serde::de::DeserializeOwned;
use serde_json::Value;
use uuid::Uuid;

use crate::dto::{
    AnswerDto, ArtifactDto, AttemptDto, BudgetDto, CostReportDto, CostRowDto, EvaluationSummaryDto,
    EventDto, FailureDto, GoalDto, MemoryItemDto, PlanDto, QuestionDto, QuestionOptionDto,
    QuestionPolicyDto, QuestionPolicyKind, RouteDto, RunDto, RunModeDto, RunStatusDto,
    RunSummaryDto, TaskCountsDto, TaskDto, TaskLogLineDto, TaskSummaryDto, UnderstandingDto,
    UsageDto, WorkerDoctorDto, WorkspaceDto,
};

/// Decodes a projection JSON column, falling back to `None` on anything odd.
fn decode<T: DeserializeOwned>(value: &Value) -> Option<T> {
    if value.is_null() {
        return None;
    }
    serde_json::from_value(value.clone()).ok()
}

/// `i64` column → `u64`, clamping negatives to zero.
fn nat(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// `i32` column → `u32`, clamping negatives to zero.
fn nat32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

/// `i32` attempt number → the `u8` the wire uses.
fn attempt_no(value: i32) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

impl From<&Usage> for UsageDto {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            cost_usd: usage.cost_usd,
            wall_ms: usage.wall_ms,
        }
    }
}

impl From<&Budget> for BudgetDto {
    fn from(budget: &Budget) -> Self {
        Self {
            max_usd: budget.max_usd,
            max_tokens: budget.max_tokens,
            max_wall_ms: budget
                .max_wall
                .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            max_attempts: budget.max_attempts,
            max_parallel: budget.max_parallel,
        }
    }
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        Self {
            worker: route.worker.as_str().to_owned(),
            model: route.model.to_string(),
            effort: route.effort.map(|e| e.to_string()),
        }
    }
}

impl From<&Workspace> for WorkspaceDto {
    fn from(workspace: &Workspace) -> Self {
        Self {
            root: workspace.root.clone(),
            kind: format!("{:?}", workspace.kind).to_lowercase(),
            base_rev: workspace.base_rev.clone(),
        }
    }
}

impl From<&Answer> for AnswerDto {
    fn from(answer: &Answer) -> Self {
        Self {
            selected: answer.selected.clone(),
            free_text: answer.free_text.clone(),
            answered_by: answer.answered_by.clone(),
        }
    }
}

impl From<&QuestionOption> for QuestionOptionDto {
    fn from(option: &QuestionOption) -> Self {
        Self {
            label: option.label.clone(),
            description: option.description.clone(),
            recommended: option.recommended,
        }
    }
}

impl From<&QuestionPolicy> for QuestionPolicyDto {
    fn from(policy: &QuestionPolicy) -> Self {
        match policy {
            QuestionPolicy::Block => Self {
                kind: QuestionPolicyKind::Block,
                timeout_ms: None,
            },
            QuestionPolicy::DefaultAfter { timeout } => Self {
                kind: QuestionPolicyKind::DefaultAfter,
                timeout_ms: Some(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
            },
        }
    }
}

/// Usage from the scalar columns every read model carries (authoritative even
/// when the JSON blob is stale).
fn usage_columns(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    wall_ms: i64,
    cost: Option<rust_decimal::Decimal>,
) -> UsageDto {
    UsageDto {
        input_tokens: nat(input),
        output_tokens: nat(output),
        cache_read_tokens: nat(cache_read),
        cache_write_tokens: nat(cache_write),
        cost_usd: cost,
        wall_ms: nat(wall_ms),
    }
}

/// `RunStatus` name → [`RunStatusDto`]; unknown names fall back to `received`.
fn run_status(name: &str) -> RunStatusDto {
    serde_json::from_value(Value::String(name.to_owned())).unwrap_or(RunStatusDto::Received)
}

/// `mode` column → [`RunModeDto`].
fn run_mode(name: &str) -> RunModeDto {
    match name {
        "headless" => RunModeDto::Headless,
        "kohral" => RunModeDto::Kohral,
        _ => RunModeDto::Interactive,
    }
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

/// `orch.run_overview` → [`RunSummaryDto`].
#[must_use]
pub fn run_summary(row: &RunOverviewRow) -> RunSummaryDto {
    RunSummaryDto {
        id: RunId::from_uuid(row.run_id),
        status: run_status(&row.status),
        goal_excerpt: row.goal_excerpt.clone(),
        usage: usage_columns(
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.wall_ms,
            row.cost_usd.get(),
        ),
        task_counts: TaskCountsDto {
            total: nat32(row.tasks_total),
            succeeded: nat32(row.tasks_succeeded),
            failed: nat32(row.tasks_failed),
            cancelled: nat32(row.tasks_cancelled),
            skipped: nat32(row.tasks_skipped),
        },
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// `orch.run_overview` (+ its tasks) → [`RunDto`].
#[must_use]
pub fn run(row: &RunOverviewRow, tasks: &[TaskBoardRow]) -> RunDto {
    RunDto {
        id: RunId::from_uuid(row.run_id),
        status: run_status(&row.status),
        goal: GoalDto {
            text: row.goal_text.clone(),
            attachments: Vec::new(),
            cwd: row.cwd.clone().into(),
            repo_kind: row.repo_kind.clone(),
        },
        mode: run_mode(&row.mode),
        budget: decode::<Budget>(&row.budget)
            .as_ref()
            .map(BudgetDto::from)
            .unwrap_or_default(),
        usage: usage_columns(
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.wall_ms,
            row.cost_usd.get(),
        ),
        understanding: row
            .understanding
            .as_ref()
            .filter(|v| !v.is_null())
            .map(|v| UnderstandingDto(v.clone())),
        plan: row
            .plan
            .as_ref()
            .filter(|v| !v.is_null())
            .map(|v| PlanDto(v.clone())),
        open_questions: row
            .open_question_ids
            .iter()
            .copied()
            .map(QuestionId::from_uuid)
            .collect(),
        tasks: tasks.iter().map(task_summary).collect(),
        evaluation: row.evaluation_id.map(|id| EvaluationSummaryDto {
            id: EvaluationId::from_uuid(id),
            overall: row.evaluation_overall,
            verdict: row.evaluation_verdict.clone(),
        }),
        created_at: row.created_at,
        updated_at: row.updated_at,
        version: nat(row.version),
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

/// The attempt objects `orch.task_board` stores in its `attempts` array.
#[derive(Debug, serde::Deserialize)]
struct AttemptJson {
    id: Uuid,
    no: u8,
    #[serde(default)]
    route: Option<Route>,
    status: String,
    #[serde(default)]
    workspace: Option<Workspace>,
    #[serde(default)]
    worker_session_id: Option<String>,
    started_at: DateTime<Utc>,
    #[serde(default)]
    ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    failure: Option<FailureJson>,
}

#[derive(Debug, serde::Deserialize)]
struct FailureJson {
    #[serde(default)]
    class: Option<Value>,
    #[serde(default)]
    message: Option<String>,
}

/// The attempts of a `orch.task_board` row, oldest first.
#[must_use]
pub fn attempts_of(row: &TaskBoardRow) -> Vec<AttemptDto> {
    let Some(array) = row.attempts.as_array() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|value| serde_json::from_value::<AttemptJson>(value.clone()).ok())
        .map(|attempt| AttemptDto {
            id: AttemptId::from_uuid(attempt.id),
            no: attempt.no,
            route: attempt.route.as_ref().map_or_else(
                || RouteDto {
                    worker: row.route_worker.clone().unwrap_or_default(),
                    model: row.route_model.clone().unwrap_or_default(),
                    effort: row.route_effort.clone(),
                },
                RouteDto::from,
            ),
            status: attempt.status,
            workspace: attempt.workspace.as_ref().map(WorkspaceDto::from),
            worker_session_id: attempt.worker_session_id,
            started_at: attempt.started_at,
            ended_at: attempt.ended_at,
            usage: attempt
                .usage
                .as_ref()
                .map(UsageDto::from)
                .unwrap_or_default(),
            failure: attempt.failure.map(|failure| FailureDto {
                class: failure.class.map_or_else(
                    || "unknown".to_owned(),
                    |value| match value {
                        Value::String(name) => name,
                        other => other
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_owned(),
                    },
                ),
                message: failure.message.unwrap_or_default(),
            }),
        })
        .collect()
}

fn task_route(row: &TaskBoardRow) -> Option<RouteDto> {
    row.route
        .as_ref()
        .and_then(decode::<Route>)
        .as_ref()
        .map(RouteDto::from)
}

/// `orch.task_board` → [`TaskSummaryDto`].
#[must_use]
pub fn task_summary(row: &TaskBoardRow) -> TaskSummaryDto {
    TaskSummaryDto {
        id: TaskId::from_uuid(row.task_id),
        kind: row.kind.clone(),
        title: row.title.clone(),
        status: row.status.clone(),
        route: task_route(row),
        attempt_count: nat32(row.attempt_count),
        usage: usage_columns(
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.wall_ms,
            row.cost_usd.get(),
        ),
    }
}

/// `orch.task_board` (+ its artifacts) → [`TaskDto`].
#[must_use]
pub fn task(row: &TaskBoardRow, artifacts: Vec<ArtifactDto>) -> TaskDto {
    TaskDto {
        id: TaskId::from_uuid(row.task_id),
        run_id: RunId::from_uuid(row.run_id),
        kind: row.kind.clone(),
        title: row.title.clone(),
        status: row.status.clone(),
        route: task_route(row),
        attempts: attempts_of(row),
        depends_on: row
            .depends_on
            .iter()
            .copied()
            .map(TaskId::from_uuid)
            .collect(),
        usage: usage_columns(
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            row.wall_ms,
            row.cost_usd.get(),
        ),
        artifacts,
        acceptance_criteria: decode::<Vec<String>>(&row.acceptance_criteria).unwrap_or_default(),
    }
}

/// `orch.task_log` → [`TaskLogLineDto`].
#[must_use]
pub fn task_log_line(row: &TaskLogRow) -> TaskLogLineDto {
    TaskLogLineDto {
        seq: nat(row.seq),
        attempt: attempt_no(row.attempt),
        at: row.at,
        kind: row.kind.clone(),
        payload: row.payload.clone(),
    }
}

// ---------------------------------------------------------------------------
// Questions, artifacts, cost
// ---------------------------------------------------------------------------

/// `orch.question_inbox` → [`QuestionDto`].
#[must_use]
pub fn question(row: &QuestionInboxRow) -> QuestionDto {
    QuestionDto {
        id: QuestionId::from_uuid(row.question_id),
        run_id: RunId::from_uuid(row.run_id),
        task_id: row.task_id.map(TaskId::from_uuid),
        text: row.text.clone(),
        options: decode::<Vec<QuestionOption>>(&row.options)
            .unwrap_or_default()
            .iter()
            .map(QuestionOptionDto::from)
            .collect(),
        multi_select: row.multi_select,
        default: row
            .default_answer
            .as_ref()
            .and_then(decode::<Answer>)
            .as_ref()
            .map(AnswerDto::from),
        policy: decode::<QuestionPolicy>(&row.policy).as_ref().map_or_else(
            || QuestionPolicyDto {
                kind: if row.policy_kind == "default_after" {
                    QuestionPolicyKind::DefaultAfter
                } else {
                    QuestionPolicyKind::Block
                },
                timeout_ms: row.timeout_ms.map(nat),
            },
            QuestionPolicyDto::from,
        ),
        status: row.status.clone(),
        answer: row
            .answer
            .as_ref()
            .and_then(decode::<Answer>)
            .as_ref()
            .map(AnswerDto::from),
        asked_at: row.asked_at,
    }
}

/// `orch.artifacts` → [`ArtifactDto`].
#[must_use]
pub fn artifact(row: &ArtifactRow) -> ArtifactDto {
    ArtifactDto {
        id: ArtifactId::from_uuid(row.artifact_id),
        run_id: RunId::from_uuid(row.run_id),
        task_id: row.task_id.map(TaskId::from_uuid),
        kind: row.kind.clone(),
        uri: row.uri.clone(),
        sha256: row.sha256.clone(),
        bytes: row.bytes.map(nat),
        produced_by: row.produced_by.clone(),
        created_at: row.created_at,
    }
}

/// `orch.cost_ledger` grouped report → [`CostReportDto`].
#[must_use]
pub fn cost_report(report: &CostReport) -> CostReportDto {
    CostReportDto {
        total_usd: report.total_usd,
        total_tokens: nat(report.total_tokens),
        rows: report
            .rows
            .iter()
            .map(|row| CostRowDto {
                key: row.key.clone(),
                usd: row.usd,
                input_tokens: nat(row.input_tokens),
                output_tokens: nat(row.output_tokens),
                attempts: u32::try_from(row.attempts).unwrap_or(u32::MAX),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// `core.events` row → [`EventDto`].
#[must_use]
pub fn event(stored: &StoredEvent) -> EventDto {
    let envelope = &stored.envelope;
    EventDto {
        position: stored.position,
        event_id: envelope.event_id,
        event_type: envelope.event_type.to_owned(),
        occurred_at: envelope.occurred_at,
        aggregate_type: envelope.aggregate_type.to_owned(),
        aggregate_id: envelope.aggregate_id,
        aggregate_version: envelope.aggregate_version,
        correlation_id: envelope.correlation_id,
        payload: envelope.payload.clone(),
    }
}

/// Bus event → [`EventDto`].
#[must_use]
pub fn bus_event(bus: &kevin_bus::BusEvent) -> EventDto {
    EventDto {
        position: bus.position,
        event_id: EventId::from_uuid(bus.envelope.event_id.as_uuid()),
        event_type: bus.envelope.event_type.to_owned(),
        occurred_at: bus.envelope.occurred_at,
        aggregate_type: bus.envelope.aggregate_type.to_owned(),
        aggregate_id: bus.envelope.aggregate_id,
        aggregate_version: bus.envelope.aggregate_version,
        correlation_id: bus.envelope.correlation_id,
        payload: bus.envelope.payload.clone(),
    }
}

// ---------------------------------------------------------------------------
// Memory and workers
// ---------------------------------------------------------------------------

/// A search hit → [`MemoryItemDto`].
#[must_use]
pub fn memory_hit(hit: &Hit) -> MemoryItemDto {
    let mut dto = memory_record(&hit.item);
    dto.similarity = Some(hit.similarity);
    dto
}

/// A stored item → [`MemoryItemDto`].
#[must_use]
pub fn memory_record(record: &kevin_memory::item::MemoryRecord) -> MemoryItemDto {
    MemoryItemDto {
        id: record.id,
        kind: record.kind.as_str().to_owned(),
        content: record.content.clone(),
        tags: record.tags.clone(),
        importance: record.importance,
        similarity: None,
        source: serde_json::to_value(&record.source).unwrap_or(Value::Null),
        created_at: record.created_at,
    }
}

/// A row of `memory.lessons_view` → [`MemoryItemDto`].
#[must_use]
pub fn lesson(lesson: &Lesson) -> MemoryItemDto {
    MemoryItemDto {
        id: MemoryItemId::from_uuid(lesson.id.as_uuid()),
        kind: "lesson".to_owned(),
        content: lesson.content.clone(),
        tags: lesson.tags.clone(),
        importance: lesson.importance,
        similarity: None,
        source: serde_json::json!({
            "run_id": lesson.run_id,
            "scope": lesson.scope,
        }),
        created_at: lesson.created_at,
    }
}

/// `Worker::doctor()` → [`WorkerDoctorDto`].
#[must_use]
pub fn worker_doctor(doctor: &Doctor, enabled: bool) -> WorkerDoctorDto {
    let mut problems: Vec<String> = doctor.notes.clone();
    if doctor.binary.is_none() && enabled {
        problems.push("binary not found on PATH".to_owned());
    }
    WorkerDoctorDto {
        kind: doctor.kind.as_str().to_owned(),
        enabled,
        binary: doctor.binary.clone(),
        version: doctor.version.clone(),
        auth_ready: match &doctor.auth_ready {
            AuthStatus::Ready => Some(true),
            AuthStatus::Missing(_) => Some(false),
            AuthStatus::Unknown => None,
        },
        problems,
    }
}

// ---------------------------------------------------------------------------
// Aggregates → DTOs (read-after-write)
// ---------------------------------------------------------------------------
//
// A command endpoint answers from the aggregate it just changed, not from the
// projection: a client that approved a plan must not be told the run is still
// `awaiting_plan_approval` because the read model has not caught up yet.

/// `RunStatus` → the wire enum.
fn run_status_dto(status: kevin_domain::RunStatus) -> RunStatusDto {
    run_status(status.as_str())
}

/// The `Run` aggregate as a [`RunDto`].
#[must_use]
pub fn run_aggregate(run: &kevin_domain::Run) -> RunDto {
    let goal = run.goal();
    RunDto {
        id: run.run_id(),
        status: run_status_dto(run.status()),
        goal: GoalDto {
            text: goal.map(|goal| goal.text.clone()).unwrap_or_default(),
            attachments: goal
                .map(|goal| {
                    goal.attachments
                        .iter()
                        .map(|artifact| crate::dto::AttachmentRef {
                            kind: artifact_kind_name(artifact.kind).to_owned(),
                            uri: artifact.uri.clone(),
                            bytes: artifact.bytes,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            cwd: goal.map(|goal| goal.cwd.clone()).unwrap_or_default(),
            repo_kind: goal.map_or_else(
                || "none".to_owned(),
                |goal| repo_kind_name(goal.repo_kind).to_owned(),
            ),
        },
        mode: run.mode().map_or(RunModeDto::Interactive, run_mode_dto),
        budget: BudgetDto::from(run.budget()),
        usage: UsageDto::from(run.usage()),
        understanding: run
            .understanding()
            .and_then(|u| serde_json::to_value(u).ok())
            .map(UnderstandingDto),
        plan: run
            .plan()
            .and_then(|plan| serde_json::to_value(plan).ok())
            .map(PlanDto),
        open_questions: run.open_question_ids().to_vec(),
        // The task board is a projection; a write response carries the ids the
        // aggregate knows, and clients follow up with `GET …/tasks`.
        tasks: Vec::new(),
        evaluation: run.evaluation_ids().last().map(|id| EvaluationSummaryDto {
            id: *id,
            overall: None,
            verdict: None,
        }),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        version: run.version(),
    }
}

/// The `Task` aggregate as a [`TaskDto`].
#[must_use]
pub fn task_aggregate(task: &kevin_domain::Task) -> TaskDto {
    let spec = task.spec();
    TaskDto {
        id: task.task_id(),
        run_id: task.run_id(),
        kind: task
            .kind()
            .map_or_else(|| "unknown".to_owned(), ToString::to_string),
        title: spec.map(|spec| spec.title.clone()).unwrap_or_default(),
        status: task.status().as_str().to_owned(),
        route: task.route().map(RouteDto::from),
        // Attempts carry timestamps the aggregate does not keep; the caller
        // overlays them from `orch.task_board` when a row exists.
        attempts: Vec::new(),
        depends_on: spec.map(|spec| spec.depends_on.clone()).unwrap_or_default(),
        usage: UsageDto::from(task.usage()),
        artifacts: Vec::new(),
        acceptance_criteria: spec
            .map(|spec| spec.acceptance_criteria.clone())
            .unwrap_or_default(),
    }
}

/// The `Question` aggregate as a [`QuestionDto`].
#[must_use]
pub fn question_aggregate(question: &kevin_domain::Question) -> QuestionDto {
    QuestionDto {
        id: question.question_id(),
        run_id: question.run_id(),
        task_id: question.task_id(),
        text: question.text().to_owned(),
        options: question
            .options()
            .iter()
            .map(QuestionOptionDto::from)
            .collect(),
        multi_select: question.multi_select(),
        default: question.default_answer().map(AnswerDto::from),
        policy: question.policy().as_ref().map_or(
            QuestionPolicyDto {
                kind: QuestionPolicyKind::Block,
                timeout_ms: None,
            },
            QuestionPolicyDto::from,
        ),
        status: question_status_name(question.status()).to_owned(),
        answer: question.answer().map(AnswerDto::from),
        asked_at: Utc::now(),
    }
}

fn run_mode_dto(mode: &kevin_domain::values::RunMode) -> RunModeDto {
    match mode {
        kevin_domain::values::RunMode::Interactive => RunModeDto::Interactive,
        kevin_domain::values::RunMode::Headless => RunModeDto::Headless,
        kevin_domain::values::RunMode::Kohral { .. } => RunModeDto::Kohral,
    }
}

fn question_status_name(status: kevin_domain::values::QuestionStatus) -> &'static str {
    match status {
        kevin_domain::values::QuestionStatus::Open => "open",
        kevin_domain::values::QuestionStatus::Answered => "answered",
        kevin_domain::values::QuestionStatus::Expired => "expired",
    }
}

fn repo_kind_name(kind: kevin_domain::values::RepoKind) -> &'static str {
    match kind {
        kevin_domain::values::RepoKind::Git => "git",
        kevin_domain::values::RepoKind::Jj => "jj",
        kevin_domain::values::RepoKind::None => "none",
    }
}

fn artifact_kind_name(kind: kevin_domain::values::ArtifactKind) -> &'static str {
    match kind {
        kevin_domain::values::ArtifactKind::Diff => "diff",
        kevin_domain::values::ArtifactKind::File => "file",
        kevin_domain::values::ArtifactKind::PrUrl => "pr_url",
        kevin_domain::values::ArtifactKind::Report => "report",
        kevin_domain::values::ArtifactKind::Json => "json",
        kevin_domain::values::ArtifactKind::Transcript => "transcript",
    }
}

/// One row of the routing leaderboard as a [`crate::dto::RouteScoreDto`].
#[must_use]
pub fn route_score(row: &kevin_router::score::LeaderboardRow) -> crate::dto::RouteScoreDto {
    crate::dto::RouteScoreDto {
        kind: row.task_kind.to_string(),
        alias: row.alias.to_string(),
        attempts: row.stats.attempts,
        successes: row.stats.successes,
        mean_quality: row.stats.mean_quality(),
        mean_cost_usd: row.stats.mean_cost_usd(),
        mean_wall_ms: row.stats.mean_wall_ms(),
        sampled_score: Some(row.stats.p_success()),
    }
}
