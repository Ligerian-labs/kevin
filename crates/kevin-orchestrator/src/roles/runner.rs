//! [`RoleRunner`] — the only place where a [`Role`] meets a
//! [`Worker`](kevin_worker::Worker).
//!
//! One role call is one single-shot worker attempt carrying the role's schema.
//! Workers that support structured output natively return it in
//! [`WorkerOutcome::Succeeded::structured`]; the others get their final text
//! run through [`kevin_worker::structured::extract_and_validate`] (the role's
//! [`Role::parse`] does both). A schema violation buys exactly **one** repair
//! turn on the same worker session (`plan/04-workers.md` §Structured output,
//! `plan/05-orchestration.md` §3.4); a second one fails the call.

use std::sync::Arc;
use std::time::Duration;

use kevin_domain::{AttemptId, Effort, ModelAlias, Route, RunId, TaskId, Usage};
use kevin_worker::registry::WorkerRegistry;
use kevin_worker::structured::{StructuredError, repair_prompt};
use kevin_worker::{
    AttemptBudget, AttemptContext, EnvAllowlist, Route as WorkerRoute, TaskAttemptRequest,
    TaskSpec as WorkerTaskSpec, Usage as WorkerUsage, WorkerOutcome, WorkerSessionId, Workspace,
};
use tokio_util::sync::CancellationToken;

use super::context::RoleContext;
use super::{Role, RoleError};

/// Extra time the worker's own budget gets on top of the role timeout, so the
/// runner's guard is the one that fires (`plan/05-orchestration.md` §4).
pub const TIMEOUT_GRACE: Duration = Duration::from_secs(5);

/// Runs Kevin's own roles through the worker registry.
#[derive(Debug, Clone)]
pub struct RoleRunner {
    workers: Arc<WorkerRegistry>,
    run_id: RunId,
    workspace: Workspace,
    cancel: CancellationToken,
}

/// One worker turn.
struct Turn {
    answer: TurnAnswer,
    usage: Usage,
    session_id: Option<WorkerSessionId>,
}

/// What a turn produced: an answer to parse, or the worker's own verdict that
/// the answer broke the schema (adapters validate `spec.output_schema`
/// themselves and fail `Permanent{schema_violation}`).
enum TurnAnswer {
    /// Raw text (or the worker's native structured output, re-serialised).
    Text(String),
    /// The worker refused its own output.
    SchemaViolation(StructuredError),
}

impl RoleRunner {
    /// A runner for `run_id` whose role calls happen in `workspace` (an
    /// in-place, read-only checkout for every role but the integrator).
    #[must_use]
    pub fn new(workers: Arc<WorkerRegistry>, run_id: RunId, workspace: Workspace) -> Self {
        Self {
            workers,
            run_id,
            workspace,
            cancel: CancellationToken::new(),
        }
    }

    /// Uses `token` (a child of the run token) so `CancelRun` stops role calls.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// The workspace role calls run in.
    #[must_use]
    pub const fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// Calls `role` on `route`, returning its parsed output and the usage of
    /// every turn it took (the repair turn included).
    pub async fn call<R: Role>(
        &self,
        role: &R,
        ctx: &RoleContext,
        route: &Route,
        effort: Option<Effort>,
        timeout: Duration,
    ) -> Result<(R::Output, Usage), RoleError> {
        let name = role.name();
        let request = role.build(ctx);
        let route = WorkerRoute {
            worker: route.worker,
            model: route.model.clone(),
            effort: effort.or(route.effort),
        };
        tracing::debug!(
            role = name,
            worker = %route.worker,
            model = %route.model,
            timeout_ms = timeout.as_millis(),
            "role call"
        );

        let mut usage = Usage::ZERO;
        let first = self
            .turn(
                role,
                &request,
                &route,
                ctx,
                timeout,
                request.user.clone(),
                None,
            )
            .await?;
        usage += first.usage;

        // Either the worker rejected its own structured output, or the role
        // parsed the answer here; both end in the same schema-violation path.
        let error = match first.answer {
            TurnAnswer::Text(raw) => match role.parse(&raw) {
                Ok(output) => return Ok((output, usage)),
                Err(err) => err,
            },
            TurnAnswer::SchemaViolation(source) => RoleError::Output { role: name, source },
        };
        let Some(structured) = error.structured() else {
            return Err(error);
        };
        if !structured.is_schema_violation() || request.schema.is_none() {
            return Err(error);
        }

        tracing::warn!(role = name, error = %error, "role output violated its schema; repairing once");
        let repair = repair_message(&request.user, structured, first.session_id.as_ref());
        let second = self
            .turn(
                role,
                &request,
                &route,
                ctx,
                timeout,
                repair,
                first.session_id,
            )
            .await?;
        usage += second.usage;
        match second.answer {
            TurnAnswer::Text(raw) => role.parse(&raw).map(|output| (output, usage)),
            TurnAnswer::SchemaViolation(source) => Err(RoleError::Output { role: name, source }),
        }
    }

    /// One worker turn: spawn, wait (guarded by `timeout`), normalise.
    async fn turn<R: Role>(
        &self,
        role: &R,
        request: &super::RoleRequest,
        route: &WorkerRoute,
        ctx: &RoleContext,
        timeout: Duration,
        message: String,
        prior_session: Option<WorkerSessionId>,
    ) -> Result<Turn, RoleError> {
        let name = role.name();
        let worker = self
            .workers
            .get(route.worker)
            .ok_or(RoleError::WorkerUnavailable {
                role: name,
                worker: route.worker,
            })?;
        let config = self.workers.config();
        let model = config
            .models
            .get(&route.model)
            .filter(|entry| entry.worker == route.worker)
            .cloned()
            .ok_or_else(|| unknown_model(name, &route.model))?;
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
            run_id: self.run_id,
            kind: role.task_kind(),
            spec: WorkerTaskSpec {
                title: name.to_owned(),
                instructions: message,
                inputs: Vec::new(),
                acceptance_criteria: Vec::new(),
                depends_on: Vec::new(),
                workspace_policy: role.workspace_policy(),
                output_schema: request.schema.clone(),
            },
            route: route.clone(),
            model,
            workspace: self.workspace.clone(),
            context: AttemptContext {
                system_prompt_append: request.system.clone(),
                memory: ctx.memory.as_ref().map(|m| m.text().to_owned()),
                prior_session,
            },
            env,
            budget: AttemptBudget::with_timeout(timeout.saturating_add(TIMEOUT_GRACE)),
            cancel: cancel.clone(),
        };

        let handle = worker
            .start(attempt)
            .await
            .map_err(|source| RoleError::Spawn { role: name, source })?;
        let session = handle.session_id.clone();
        let Ok(outcome) = tokio::time::timeout(timeout, handle.wait()).await else {
            cancel.cancel();
            return Err(RoleError::Timeout {
                role: name,
                timeout,
            });
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
            // The worker's own budget is the backstop for the runner's guard.
            WorkerOutcome::Failed { message, .. } if is_timeout(&message) => {
                Err(RoleError::Timeout {
                    role: name,
                    timeout,
                })
            }
            // The adapter validated `spec.output_schema` and refused the
            // answer: that is the one failure the runner repairs.
            WorkerOutcome::Failed { message, usage, .. } if is_schema_violation(&message) => {
                Ok(Turn {
                    answer: TurnAnswer::SchemaViolation(StructuredError::SchemaViolation {
                        errors: vec![message],
                    }),
                    usage: to_domain_usage(&usage),
                    session_id: session.borrow().clone(),
                })
            }
            WorkerOutcome::Failed {
                class,
                message,
                usage,
                ..
            } => Err(RoleError::WorkerFailed {
                role: name,
                class,
                message,
                usage: to_domain_usage(&usage),
            }),
        }
    }
}

/// `UnknownModel` for an alias that is missing or bound to another worker.
fn unknown_model(role: &'static str, alias: &ModelAlias) -> RoleError {
    RoleError::UnknownModel {
        role,
        alias: alias.clone(),
    }
}

/// The follow-up message of the single repair turn. On a worker session that
/// can be resumed the repair instruction alone is enough
/// (`plan/04-workers.md` §Structured output); without a session the turn
/// starts fresh, so the original request is repeated with it.
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

/// Whether an adapter's failure message reports a structured-output violation
/// (`Failed{Permanent, "schema_violation: …"}`).
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
