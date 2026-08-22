//! [`crate::port::RuntimePort`] over the real orchestrator (WS-08).
//!
//! Every write the HTTP API performs is a command on one of the three
//! application services, issued with the request's `Idempotency-Key` as
//! `command_id` — so `core.processed_commands` makes the write exactly-once
//! across retries, restarts and duplicate clients, exactly as `plan/07`
//! §Conventions promises.
//!
//! Reads-after-write are served from the **aggregate**, not from the read
//! models: a client that just approved a plan must not be told the run is
//! still `awaiting_plan_approval` because the projection has not caught up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kevin_domain::Aggregate;
use kevin_domain::ids::ArtifactId;
use kevin_domain::ids::{QuestionId, RunId, TaskId};
use kevin_domain::question::AnswerQuestion;
use kevin_domain::run::{ApprovePlan, CancelRun, Evaluate, RejectPlan, StartRun};
use kevin_domain::task::{CancelTask, RetryTask};
use kevin_domain::values::{Answer, ArtifactKind, ArtifactRef, Budget, Goal, RepoKind, RunMode};
use kevin_orchestrator::projections::{ReadModels, TaskQuery};
use kevin_orchestrator::services::CommandContext;
use kevin_orchestrator::{AppError, Handle};

use crate::convert;
use crate::dto::{
    AnswerRequest, AttachmentRef, BudgetDto, CreateRunRequest, DrainStatusDto, QuestionDto, RunDto,
    RunModeDto, TaskDto,
};
use crate::port::{CommandCtx, PortResult, Readiness, RuntimeError, RuntimePort};

/// How long `/readyz` waits for the database ping (plan/10 §Health and drain).
const DB_PING_TIMEOUT: Duration = Duration::from_secs(1);

/// The production [`RuntimePort`].
#[derive(Clone)]
pub struct OrchestratorRuntime {
    handle: Arc<Handle>,
    read: ReadModels,
}

impl std::fmt::Debug for OrchestratorRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorRuntime")
            .field("admitting", &self.handle.is_admitting())
            .field("active_runs", &self.handle.active_runs())
            .finish_non_exhaustive()
    }
}

impl OrchestratorRuntime {
    /// Wraps a booted orchestrator and the read models the drain report and
    /// the readiness probe query.
    #[must_use]
    pub const fn new(handle: Arc<Handle>, read: ReadModels) -> Self {
        Self { handle, read }
    }

    /// The orchestrator this port drives.
    #[must_use]
    pub fn handle(&self) -> &Arc<Handle> {
        &self.handle
    }

    /// Translates an API command context into the orchestrator's.
    fn ctx(ctx: &CommandCtx, correlation_id: RunId) -> CommandContext {
        CommandContext::new(ctx.command_id, ctx.actor.clone(), correlation_id)
    }

    /// The state a caller should see right after a run command.
    ///
    /// The **aggregate** is authoritative for everything a command just
    /// changed (status, version, plan, budget); the **read model** supplies
    /// what the aggregate does not keep — the event timestamps and the task
    /// board. When the projection has not caught up (or has no row yet, right
    /// after `POST /runs`) the aggregate answer still stands on its own.
    async fn run_view(&self, run_id: RunId) -> PortResult<RunDto> {
        let run = self.handle.run_service().load(run_id).await?;
        if run.version() == 0 {
            return Err(RuntimeError::RunNotFound(run_id));
        }
        let mut dto = convert::run_aggregate(&run);
        if let Ok(Some(row)) = self.read.run(run_id.as_uuid()).await {
            dto.created_at = row.created_at;
            dto.updated_at = row.updated_at;
            if let Some(id) = row.evaluation_id {
                dto.evaluation = Some(crate::dto::EvaluationSummaryDto {
                    id: kevin_domain::ids::EvaluationId::from_uuid(id),
                    overall: row.evaluation_overall,
                    verdict: row.evaluation_verdict.clone(),
                });
            }
        }
        if let Ok(tasks) = self.read.tasks_of_run(run_id.as_uuid()).await {
            dto.tasks = tasks.iter().map(convert::task_summary).collect();
        }
        Ok(dto)
    }

    /// The state a caller should see right after a task command.
    async fn task_view(&self, task_id: TaskId) -> PortResult<TaskDto> {
        let task = self.handle.task_service().load(task_id).await?;
        if task.version() == 0 {
            return Err(RuntimeError::TaskNotFound(task_id));
        }
        let mut dto = convert::task_aggregate(&task);
        if let Ok(Some(row)) = self.read.task(task_id.as_uuid()).await {
            dto.attempts = convert::attempts_of(&row);
        }
        if let Ok(artifacts) = self.read.artifacts_of_task(task_id.as_uuid()).await {
            dto.artifacts = artifacts.iter().map(convert::artifact).collect();
        }
        Ok(dto)
    }

    /// The state a caller should see right after answering a question.
    async fn question_view(&self, question_id: QuestionId) -> PortResult<QuestionDto> {
        let question = self.handle.question_service().load(question_id).await?;
        if question.version() == 0 {
            return Err(RuntimeError::QuestionNotFound(question_id));
        }
        let mut dto = convert::question_aggregate(&question);
        if let Ok(Some(row)) = self.read.question(question_id.as_uuid()).await {
            dto.asked_at = row.asked_at;
        }
        Ok(dto)
    }

    /// The name recorded as `by`/`requested_by` on the command.
    fn actor_name(ctx: &CommandCtx) -> String {
        match &ctx.actor {
            kevin_domain::Actor::User { name } => name.clone(),
            kevin_domain::Actor::System { component } => component.clone(),
            kevin_domain::Actor::Worker { kind } => kind.to_string(),
            kevin_domain::Actor::Kohral { agent_id } => agent_id.clone(),
        }
    }
}

/// `AppError` → the API's port error, preserving the stable codes.
impl From<AppError> for RuntimeError {
    fn from(err: AppError) -> Self {
        match err {
            AppError::NotFound { aggregate, id } => match aggregate {
                "task" => RuntimeError::TaskNotFound(TaskId::from_uuid(id)),
                "question" => RuntimeError::QuestionNotFound(QuestionId::from_uuid(id)),
                _ => RuntimeError::RunNotFound(RunId::from_uuid(id)),
            },
            AppError::Domain(domain) => RuntimeError::Domain(domain),
            AppError::Conflict { stream, attempts } => RuntimeError::Internal(format!(
                "optimistic concurrency conflict on {stream} after {attempts} attempts"
            )),
            AppError::Store(store) => RuntimeError::Storage(store.to_string()),
            AppError::Duplicate(value) => {
                RuntimeError::Internal(format!("command already processed: {value}"))
            }
            AppError::Corrupt { stream, message } => {
                RuntimeError::Internal(format!("corrupt stream {stream}: {message}"))
            }
            AppError::Port(port) => {
                // The engine reports "not admitting new runs" as a transient
                // port error; the API has a dedicated 503 code for it.
                if port.to_string().contains("draining") {
                    RuntimeError::Draining
                } else {
                    RuntimeError::Unavailable(port.to_string())
                }
            }
            other => RuntimeError::Internal(other.to_string()),
        }
    }
}

#[async_trait]
impl RuntimePort for OrchestratorRuntime {
    async fn start_run(&self, request: CreateRunRequest, ctx: CommandCtx) -> PortResult<RunDto> {
        if !self.handle.is_admitting() {
            return Err(RuntimeError::Draining);
        }
        let config = &self.handle.deps().config;
        let run_id = RunId::from_uuid(ctx.command_id.as_uuid());
        let cwd = request
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let cmd = StartRun {
            run_id,
            goal: Goal {
                text: request.goal.clone(),
                attachments: request.attachments.iter().map(attachment).collect(),
                repo_kind: repo_kind(&cwd),
                cwd,
            },
            mode: match request.mode.unwrap_or_default() {
                RunModeDto::Headless => RunMode::Headless,
                // A Kohral run is never started through this surface; the
                // Kohral gateway owns that mode (plan/08).
                RunModeDto::Interactive | RunModeDto::Kohral => RunMode::Interactive,
            },
            budget: budget_from(request.budget.as_ref(), &config.budget),
            requested_by: Self::actor_name(&ctx),
            auto_approve_plans: config.kevin.auto_approve_plans,
        };

        let started = self.handle.start_run(cmd, &Self::ctx(&ctx, run_id)).await?;
        self.run_view(started).await
    }

    async fn cancel_run(
        &self,
        run_id: RunId,
        reason: Option<String>,
        ctx: CommandCtx,
    ) -> PortResult<RunDto> {
        self.handle
            .run_service()
            .cancel(
                run_id,
                CancelRun {
                    by: Self::actor_name(&ctx),
                    reason: reason.unwrap_or_else(|| "cancelled through the API".to_owned()),
                },
                &Self::ctx(&ctx, run_id),
            )
            .await?;
        self.run_view(run_id).await
    }

    async fn approve_plan(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<RunDto> {
        self.handle
            .run_service()
            .approve_plan(
                run_id,
                ApprovePlan {
                    by: Self::actor_name(&ctx),
                },
                &Self::ctx(&ctx, run_id),
            )
            .await?;
        self.run_view(run_id).await
    }

    async fn reject_plan(
        &self,
        run_id: RunId,
        feedback: String,
        ctx: CommandCtx,
    ) -> PortResult<RunDto> {
        self.handle
            .run_service()
            .reject_plan(
                run_id,
                RejectPlan {
                    by: Self::actor_name(&ctx),
                    feedback,
                },
                &Self::ctx(&ctx, run_id),
            )
            .await?;
        self.run_view(run_id).await
    }

    async fn evaluate_run(&self, run_id: RunId, ctx: CommandCtx) -> PortResult<()> {
        self.handle
            .run_service()
            .evaluate(
                run_id,
                Evaluate {
                    requested_by: Self::actor_name(&ctx),
                },
                &Self::ctx(&ctx, run_id),
            )
            .await?;
        Ok(())
    }

    async fn retry_task(
        &self,
        task_id: TaskId,
        exclude_route: bool,
        ctx: CommandCtx,
    ) -> PortResult<TaskDto> {
        let task = self.handle.task_service().load(task_id).await?;
        if task.version() == 0 {
            return Err(RuntimeError::TaskNotFound(task_id));
        }
        // `exclude_route` is advisory: the saga re-routes a retried task and
        // the router excludes the failing alias when the reason says so.
        let reason = if exclude_route {
            "retried through the API (exclude the failing route)".to_owned()
        } else {
            "retried through the API".to_owned()
        };
        self.handle
            .task_service()
            .retry_task(
                task_id,
                RetryTask { reason },
                &Self::ctx(&ctx, task.run_id()),
            )
            .await?;
        self.task_view(task_id).await
    }

    async fn cancel_task(&self, task_id: TaskId, ctx: CommandCtx) -> PortResult<TaskDto> {
        let task = self.handle.task_service().load(task_id).await?;
        if task.version() == 0 {
            return Err(RuntimeError::TaskNotFound(task_id));
        }
        self.handle
            .task_service()
            .cancel_task(
                task_id,
                CancelTask {
                    reason: "cancelled through the API".to_owned(),
                },
                &Self::ctx(&ctx, task.run_id()),
            )
            .await?;
        self.task_view(task_id).await
    }

    async fn answer_question(
        &self,
        question_id: QuestionId,
        answer: AnswerRequest,
        ctx: CommandCtx,
    ) -> PortResult<QuestionDto> {
        let question = self.handle.question_service().load(question_id).await?;
        if question.version() == 0 {
            return Err(RuntimeError::QuestionNotFound(question_id));
        }
        self.handle
            .question_service()
            .answer(
                question_id,
                AnswerQuestion {
                    answer: Answer {
                        selected: answer.selected,
                        free_text: answer.free_text,
                        answered_by: Self::actor_name(&ctx),
                    },
                },
                &Self::ctx(&ctx, question.run_id()),
            )
            .await?;
        self.question_view(question_id).await
    }

    async fn set_drain(&self, draining: bool) -> PortResult<DrainStatusDto> {
        if draining {
            self.handle.drain().await;
        } else {
            self.handle.supervisor().undrain();
        }
        self.drain_status().await
    }

    async fn drain_status(&self) -> PortResult<DrainStatusDto> {
        let running_attempts = self
            .read
            .tasks(&TaskQuery {
                status: Some("running".to_owned()),
                ..TaskQuery::default()
            })
            .await
            .map_or(0, |page| u32::try_from(page.len()).unwrap_or(u32::MAX));
        Ok(DrainStatusDto {
            draining: !self.handle.is_admitting(),
            running_runs: u32::try_from(self.handle.active_runs()).unwrap_or(u32::MAX),
            running_attempts,
        })
    }

    async fn readiness(&self) -> Readiness {
        let db = tokio::time::timeout(
            DB_PING_TIMEOUT,
            sqlx::query("SELECT 1").execute(self.read.pool()),
        )
        .await
        .is_ok_and(|result| result.is_ok());

        Readiness {
            db,
            draining: !self.handle.is_admitting(),
            // The registry is built at boot and `doctor` spawns subprocesses,
            // so readiness checks that workers were *registered*, not that
            // every CLI is authenticated (that is `GET /api/v1/workers`).
            workers_ok: !self.handle.deps().workers.kinds().is_empty(),
        }
    }
}

/// `[budget]` defaults, overridden by whatever the request set.
fn budget_from(request: Option<&BudgetDto>, defaults: &kevin_config::schema::Budget) -> Budget {
    let mut budget = Budget {
        max_usd: Some(defaults.default_run_usd),
        max_tokens: None,
        max_wall: Some(defaults.default_run_wall),
        max_attempts: defaults.max_attempts,
        max_parallel: defaults.max_parallel_tasks,
    };
    let Some(request) = request else {
        return budget;
    };
    if let Some(max_usd) = request.max_usd {
        budget.max_usd = Some(max_usd);
    }
    if let Some(max_tokens) = request.max_tokens {
        budget.max_tokens = Some(max_tokens);
    }
    if let Some(max_wall_ms) = request.max_wall_ms {
        budget.max_wall = Some(Duration::from_millis(max_wall_ms));
    }
    if request.max_attempts > 0 {
        budget.max_attempts = request.max_attempts;
    }
    if request.max_parallel > 0 {
        budget.max_parallel = request.max_parallel;
    }
    budget
}

/// `.jj` wins over `.git` when a repository is colocated (plan/02 `RepoKind`).
fn repo_kind(cwd: &Path) -> RepoKind {
    if cwd.join(".jj").exists() {
        RepoKind::Jj
    } else if cwd.join(".git").exists() {
        RepoKind::Git
    } else {
        RepoKind::None
    }
}

/// An attachment reference from the wire.
fn attachment(reference: &AttachmentRef) -> ArtifactRef {
    ArtifactRef {
        id: ArtifactId::new(),
        kind: match reference.kind.as_str() {
            "diff" => ArtifactKind::Diff,
            "pr_url" => ArtifactKind::PrUrl,
            "report" => ArtifactKind::Report,
            "json" => ArtifactKind::Json,
            "transcript" => ArtifactKind::Transcript,
            _ => ArtifactKind::File,
        },
        uri: reference.uri.clone(),
        sha256: None,
        bytes: reference.bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::{budget_from, repo_kind};
    use crate::dto::BudgetDto;
    use kevin_domain::values::RepoKind;

    #[test]
    fn the_request_budget_overrides_the_config_defaults() {
        let defaults = kevin_config::schema::Budget::default();
        let plain = budget_from(None, &defaults);
        assert_eq!(plain.max_usd, Some(defaults.default_run_usd));
        assert_eq!(plain.max_attempts, defaults.max_attempts);

        let overridden = budget_from(
            Some(&BudgetDto {
                max_usd: Some(rust_decimal::Decimal::new(500, 2)),
                max_tokens: Some(1000),
                max_wall_ms: Some(60_000),
                max_attempts: 7,
                max_parallel: 0,
            }),
            &defaults,
        );
        assert_eq!(overridden.max_usd, Some(rust_decimal::Decimal::new(500, 2)));
        assert_eq!(overridden.max_tokens, Some(1000));
        assert_eq!(overridden.max_attempts, 7);
        assert_eq!(
            overridden.max_parallel, defaults.max_parallel_tasks,
            "a zero in the request means `unset`, not `zero`"
        );
    }

    #[test]
    fn jj_wins_over_git_when_colocated() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(repo_kind(dir.path()), RepoKind::None);
        std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
        assert_eq!(repo_kind(dir.path()), RepoKind::Git);
        std::fs::create_dir(dir.path().join(".jj")).expect("mkdir");
        assert_eq!(repo_kind(dir.path()), RepoKind::Jj);
    }
}
