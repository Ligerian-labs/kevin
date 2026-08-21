//! [`RoleRunnerRoles`] — the production [`RolesPort`] over WS-10's
//! [`RoleRunner`](crate::roles::RoleRunner).
//!
//! The saga speaks the small [`crate::ports::RoleContext`]: the facts it holds
//! about the run. WS-10's [`roles::RoleContext`] is the prompt-side view of the
//! same facts. This adapter is the (total) translation between the two, so the
//! saga never has to know how a prompt is rendered and WS-10 never has to know
//! about aggregates.

use std::sync::Arc;

use async_trait::async_trait;
use kevin_config::KevinConfig;
use kevin_domain::{Plan, Understanding, Usage};
use kevin_worker::WorkerRegistry;
use tokio_util::sync::CancellationToken;

use crate::ports::{
    IntegrateContext, IntegrationSummary, PortError, PortResult, RoleContext as SagaContext,
    RolesPort,
};
use crate::roles::{
    BudgetHints, FeedbackSource, IntegrationStatus, Integrator, PlanFeedback, PlannerPlan,
    PlannerUnderstanding, PriorAnswer, RepoFacts, RoleContext, RoleError, RoleLimits, RoleRunner,
};

/// Runs Kevin's own roles through the worker registry.
#[derive(Debug, Clone)]
pub struct RoleRunnerRoles {
    workers: Arc<WorkerRegistry>,
    config: Arc<KevinConfig>,
    cancel: CancellationToken,
}

impl RoleRunnerRoles {
    /// Role calls run read-only, in place, in the run's `cwd`.
    #[must_use]
    pub fn new(workers: Arc<WorkerRegistry>, config: Arc<KevinConfig>) -> Self {
        Self {
            workers,
            config,
            cancel: CancellationToken::new(),
        }
    }

    /// Uses `token` (a child of the runtime root) so shutdown stops role calls.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    fn limits(&self) -> RoleLimits {
        let orchestrator = &self.config.orchestrator;
        RoleLimits {
            question_confidence_threshold: threshold(orchestrator.question_confidence_threshold),
            max_questions_per_run: as_usize(orchestrator.max_questions_per_run),
            max_tasks_per_run: as_usize(orchestrator.max_tasks_per_run),
            question_default_timeout: orchestrator.question_default_timeout,
            role_call_timeout: orchestrator.role_call_timeout,
            plan_revision_limit: orchestrator.plan_revision_limit,
            memory_context_max_tokens: as_usize(self.config.memory.context_max_tokens),
        }
    }

    /// The prompt-side context for a saga context.
    fn context(&self, ctx: &SagaContext) -> RoleContext {
        let goal = &ctx.goal;
        let repo_name = goal.cwd.file_name().map_or_else(
            || "workspace".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let mut repo = RepoFacts::new(repo_name, goal.cwd.to_string_lossy());
        repo.vcs = goal.repo_kind;
        repo.checks.clone_from(&self.config.checks.commands);

        let mut role_ctx = RoleContext::new(goal.text.clone())
            .with_run_mode(ctx.mode.clone())
            .with_repo(repo)
            .with_limits(self.limits())
            .with_budget(BudgetHints::default())
            .with_prior_answers(ctx.answers.iter().map(|a| PriorAnswer {
                question: a.question.clone(),
                answer: a.answer.clone(),
                answered_by: a.answered_by.clone(),
            }));
        role_ctx.system_context.clone_from(&ctx.system_context);
        if let Some(memory) = &ctx.memory {
            role_ctx = role_ctx.with_memory(crate::roles::MemoryBlock::new(
                memory,
                self.limits().memory_context_max_tokens,
            ));
        }
        if let Some(understanding) = &ctx.understanding {
            role_ctx = role_ctx.with_understanding(understanding.clone());
            role_ctx =
                role_ctx.with_acceptance_criteria(understanding.success_criteria.iter().cloned());
        }
        if let Some(plan) = &ctx.previous_plan {
            role_ctx = role_ctx.with_plan(plan.clone());
        }
        let mut feedback = Vec::new();
        if let Some(text) = &ctx.feedback {
            feedback.push(PlanFeedback::rejected(1, text.clone()));
        }
        if !ctx.repair_errors.is_empty() {
            feedback.push(PlanFeedback {
                revision: u32::try_from(feedback.len() + 1).unwrap_or(1),
                source: FeedbackSource::Validator,
                points: ctx.repair_errors.clone(),
            });
        }
        role_ctx.with_plan_feedback(feedback)
    }

    fn runner(&self, ctx: &SagaContext) -> RoleRunner {
        RoleRunner::new(
            Arc::clone(&self.workers),
            ctx.run_id,
            kevin_worker::Workspace::in_place(ctx.goal.cwd.clone()),
        )
        .with_cancel(self.cancel.child_token())
    }
}

#[async_trait]
impl RolesPort for RoleRunnerRoles {
    async fn understanding(&self, ctx: &SagaContext) -> PortResult<(Understanding, Usage)> {
        self.runner(ctx)
            .call(
                &PlannerUnderstanding,
                &self.context(ctx),
                &ctx.route,
                ctx.effort,
                self.config.orchestrator.role_call_timeout,
            )
            .await
            .map_err(|err| port_error(&err))
    }

    async fn plan(&self, ctx: &SagaContext) -> PortResult<(Plan, Usage)> {
        let planner = PlannerPlan::new(as_usize(self.config.orchestrator.max_tasks_per_run));
        self.runner(ctx)
            .call(
                &planner,
                &self.context(ctx),
                &ctx.route,
                ctx.effort,
                self.config.orchestrator.role_call_timeout,
            )
            .await
            .map_err(|err| port_error(&err))
    }

    async fn integrate(&self, ctx: &IntegrateContext) -> PortResult<(IntegrationSummary, Usage)> {
        let saga_ctx = SagaContext {
            run_id: ctx.run_id,
            goal: ctx.goal.clone(),
            mode: kevin_domain::RunMode::Headless,
            memory: None,
            system_context: Vec::new(),
            understanding: None,
            answers: Vec::new(),
            previous_plan: None,
            feedback: None,
            repair_errors: Vec::new(),
            route: ctx.route.clone(),
            effort: None,
        };
        let mut role_ctx = self.context(&saga_ctx);
        role_ctx = role_ctx.with_acceptance_criteria(ctx.acceptance_criteria.iter().cloned());
        role_ctx.integration.mode = integration_mode(self.config.workspace.integration);
        role_ctx.integration.pr_per_task = self.config.workspace.pr_per_task;
        role_ctx
            .integration
            .checks
            .clone_from(&self.config.checks.commands);
        role_ctx.integration.conflicts.clone_from(&ctx.conflicts);
        let (report, usage) = self
            .runner(&saga_ctx)
            .call(
                &Integrator,
                &role_ctx,
                &ctx.route,
                None,
                self.config.orchestrator.role_call_timeout,
            )
            .await
            .map_err(|err| port_error(&err))?;
        if report.status == IntegrationStatus::Failed {
            return Err(PortError::permanent("roles", report.summary));
        }
        Ok((
            IntegrationSummary {
                summary: report.summary,
            },
            usage,
        ))
    }
}

fn port_error(err: &RoleError) -> PortError {
    let class = match err {
        RoleError::Timeout { .. } | RoleError::Spawn { .. } => {
            kevin_domain::FailureClass::Transient
        }
        RoleError::WorkerFailed { class, .. } => *class,
        _ => kevin_domain::FailureClass::Permanent,
    };
    PortError {
        port: "roles",
        message: err.to_string(),
        class,
    }
}

fn integration_mode(mode: kevin_config::Integration) -> String {
    match mode {
        kevin_config::Integration::Pr => "pr",
        kevin_config::Integration::Merge => "merge",
        kevin_config::Integration::None => "none",
    }
    .to_owned()
}

#[allow(clippy::cast_possible_truncation)]
fn threshold(value: f64) -> f32 {
    value as f32
}

fn as_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
