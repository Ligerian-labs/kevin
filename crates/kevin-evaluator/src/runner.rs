//! [`JudgeRunner`] — the only place where the [`Judge`] meets a worker.
//!
//! One judge call is one single-shot worker attempt carrying the
//! rubric-specialised `kevin.evaluation.v1` schema, run in a **read-only**
//! workspace (`plan/06-memory-and-learning.md` §3.2: the judge inspects the
//! result, it never changes it). A schema violation buys exactly one repair
//! turn on the same session (`plan/04-workers.md` §Structured output); a second
//! one fails the call.

use std::sync::Arc;
use std::time::Duration;

use kevin_domain::{AttemptId, Effort, ModelAlias, Route, RunId, TaskId, TaskKind, Usage};
use kevin_worker::registry::WorkerRegistry;
use kevin_worker::structured::{StructuredError, repair_prompt};
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, Route as WorkerRoute, TaskAttemptRequest,
    TaskSpec as WorkerTaskSpec, Usage as WorkerUsage, WorkerOutcome, WorkerSessionId, Workspace,
    WorkspacePolicy,
};
use tokio_util::sync::CancellationToken;

use crate::error::{EvaluatorError, Result};
use crate::judge::{Judge, JudgeContext, JudgeOutput, JudgeOutputError, JudgeRequest};

/// Extra time the worker's own budget gets on top of the judge timeout, so the
/// runner's guard is the one that fires.
pub const TIMEOUT_GRACE: Duration = Duration::from_secs(5);

/// Runs the judge through the worker registry.
#[derive(Debug, Clone)]
pub struct JudgeRunner {
    workers: Arc<WorkerRegistry>,
    workspace: Workspace,
    cancel: CancellationToken,
}

/// One worker turn.
struct Turn {
    answer: TurnAnswer,
    usage: Usage,
    session_id: Option<WorkerSessionId>,
}

/// What a turn produced.
enum TurnAnswer {
    /// Raw text (or the worker's native structured output, re-serialised).
    Text(String),
    /// The worker refused its own output.
    SchemaViolation(StructuredError),
}

impl JudgeRunner {
    /// A runner whose judge calls happen in `workspace` (a read-only checkout
    /// of the result being judged).
    #[must_use]
    pub fn new(workers: Arc<WorkerRegistry>, workspace: Workspace) -> Self {
        Self {
            workers,
            workspace,
            cancel: CancellationToken::new(),
        }
    }

    /// Uses `token` (a child of the run token) so `CancelRun` stops judge calls.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// The workspace judge calls run in.
    #[must_use]
    pub const fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Runs the judge on `route`, returning its parsed answer and the usage of
    /// every turn (the repair turn included).
    pub async fn call(
        &self,
        ctx: &JudgeContext,
        run_id: RunId,
        route: &Route,
        effort: Option<Effort>,
        timeout: Duration,
    ) -> Result<(JudgeOutput, Usage)> {
        let request = Judge.build(ctx);
        let route = WorkerRoute {
            worker: route.worker,
            model: route.model.clone(),
            effort: effort.or(route.effort),
        };
        tracing::debug!(
            role = Judge::NAME,
            worker = %route.worker,
            rubric = %ctx.rubric.id,
            timeout_ms = timeout.as_millis(),
            "judge call"
        );

        let mut usage = Usage::ZERO;
        let first = self
            .turn(
                &request,
                &route,
                run_id,
                timeout,
                request.user.clone(),
                None,
            )
            .await?;
        usage += first.usage;

        let error: JudgeOutputError = match first.answer {
            TurnAnswer::Text(raw) => match Judge.parse(&raw, &ctx.rubric) {
                Ok(output) => return Ok((output, usage)),
                Err(err) => err,
            },
            TurnAnswer::SchemaViolation(source) => JudgeOutputError::Structured(source),
        };
        let Some(structured) = error.structured().filter(|e| e.is_schema_violation()) else {
            return Err(error.into());
        };

        tracing::warn!(role = Judge::NAME, error = %error, "judge output violated its schema; repairing once");
        let repair = repair_message(&request.user, structured, first.session_id.as_ref());
        let second = self
            .turn(&request, &route, run_id, timeout, repair, first.session_id)
            .await?;
        usage += second.usage;
        match second.answer {
            TurnAnswer::Text(raw) => Judge
                .parse(&raw, &ctx.rubric)
                .map(|output| (output, usage))
                .map_err(Into::into),
            TurnAnswer::SchemaViolation(source) => Err(JudgeOutputError::Structured(source).into()),
        }
    }

    /// One worker turn: spawn, wait (guarded by `timeout`), normalise.
    async fn turn(
        &self,
        request: &JudgeRequest,
        route: &WorkerRoute,
        run_id: RunId,
        timeout: Duration,
        message: String,
        prior_session: Option<WorkerSessionId>,
    ) -> Result<Turn> {
        let worker = self
            .workers
            .get(route.worker)
            .ok_or(EvaluatorError::WorkerUnavailable {
                worker: route.worker,
            })?;
        let config = self.workers.config();
        let model = config
            .models
            .get(&route.model)
            .filter(|entry| entry.worker == route.worker)
            .cloned()
            .ok_or_else(|| unknown_model(&route.model))?;
        let env = config
            .workers
            .get(&route.worker)
            .map_or_else(EnvAllowlist::default, |worker| {
                EnvAllowlist::build(&worker.env_passthrough, &config.env_allowlist_extra)
            });

        let cancel = self.cancel.child_token();
        let attempt = TaskAttemptRequest {
            attempt_id: AttemptId::new(),
            task_id: TaskId::new(),
            run_id,
            kind: TaskKind::Evaluate,
            spec: WorkerTaskSpec {
                title: Judge::NAME.to_owned(),
                instructions: message,
                inputs: Vec::new(),
                acceptance_criteria: Vec::new(),
                depends_on: Vec::new(),
                workspace_policy: WorkspacePolicy::ReadOnly,
                output_schema: Some(request.schema.clone()),
            },
            route: route.clone(),
            model,
            workspace: self.workspace.clone(),
            context: AttemptContext {
                system_prompt_append: request.system.clone(),
                memory: None,
                prior_session,
            },
            env,
            budget: AttemptBudget::with_timeout(timeout.saturating_add(TIMEOUT_GRACE)),
            cancel: cancel.clone(),
        };

        let handle = worker.start(attempt).await.map_err(EvaluatorError::Spawn)?;
        let session = handle.session_id.clone();
        let Ok(outcome) = tokio::time::timeout(timeout, handle.wait()).await else {
            cancel.cancel();
            return Err(EvaluatorError::Timeout(timeout));
        };

        match outcome {
            WorkerOutcome::Succeeded {
                text,
                structured,
                usage,
                session_id: final_session,
                ..
            } => Ok(Turn {
                answer: TurnAnswer::Text(structured.map_or(text, |value| value.to_string())),
                usage: to_domain_usage(&usage),
                session_id: final_session.or_else(|| session.borrow().clone()),
            }),
            WorkerOutcome::Failed { message, .. } if is_timeout(&message) => {
                Err(EvaluatorError::Timeout(timeout))
            }
            WorkerOutcome::Failed { message, usage, .. } if is_schema_violation(&message) => {
                Ok(Turn {
                    answer: TurnAnswer::SchemaViolation(StructuredError::SchemaViolation {
                        errors: vec![message],
                    }),
                    usage: to_domain_usage(&usage),
                    session_id: session.borrow().clone(),
                })
            }
            WorkerOutcome::Failed { class, message, .. } => {
                Err(EvaluatorError::JudgeFailed { class, message })
            }
        }
    }
}

/// `UnknownModel` for an alias that is missing or bound to another worker.
fn unknown_model(alias: &ModelAlias) -> EvaluatorError {
    EvaluatorError::UnknownModel {
        alias: alias.clone(),
    }
}

/// The follow-up message of the single repair turn.
fn repair_message(
    user: &str,
    error: &StructuredError,
    session: Option<&WorkerSessionId>,
) -> String {
    let repair = repair_prompt(error);
    match session {
        Some(_) => repair,
        None => format!("{user}\n\n---\n\n{repair}"),
    }
}

/// Whether an adapter's failure message reports a structured-output violation.
fn is_schema_violation(message: &str) -> bool {
    message.trim_start().starts_with("schema_violation")
}

/// Whether an adapter's failure message means "the attempt timed out".
fn is_timeout(message: &str) -> bool {
    let message = message.trim().to_ascii_lowercase();
    message == "timeout" || message.contains("timed out")
}

/// Worker usage → domain usage (identical fields, two crates until WS-05's
/// `TODO(ws-01)` swap lands).
fn to_domain_usage(usage: &WorkerUsage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost_usd: usage.cost_usd,
        wall_ms: usage.wall_ms,
    }
}
