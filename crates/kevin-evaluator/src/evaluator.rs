//! [`Evaluator::evaluate`] — judge one subject, record it, auto-apply what
//! policy allows (`plan/06-memory-and-learning.md` §3).
//!
//! The flow, in order:
//!
//! 1. gate on `evaluation.enabled` / `evaluation.evaluate_tasks`;
//! 2. pick the rubric (task kind → built-in, run → `evaluation.rubric`);
//! 3. pick a judge route that differs from the executor's when candidates allow
//!    (anti-gaming, §3.2);
//! 4. call the judge with scrubbed evidence;
//! 5. **recompute** `overall` from the rubric weights and reconcile the verdict
//!    — the judge's own arithmetic is logged, never trusted;
//! 6. record `evaluation.recorded` + the two projection rows;
//! 7. run [`AutoApply`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kevin_config::{Evaluation as EvaluationCfg, KevinConfig, ModelEntry, Roles};
use kevin_domain::{
    AttemptId, Effort, EvaluationId, EvaluationSubject, MemoryScope, ModelAlias, Proposal,
    ProposalId, ProposalStatus, Route, RubricScore, RunId, TaskKind, Usage, Verdict, WorkerKind,
};
use kevin_worker::registry::WorkerRegistry;

use crate::auto_apply::{AutoApply, AutoApplyContext, AutoApplyReport};
use crate::error::{EvaluatorError, Result, SkipReason};
use crate::evidence::{Evidence, Scrubber};
use crate::judge::{JudgeContext, JudgeOutput};
use crate::repo::{EvaluationRecord, EvaluationRepo};
use crate::router_port::OutcomeAttempt;
use crate::rubric::{self, Rubric};
use crate::runner::JudgeRunner;

/// `kevin_eval_overall_score` (`plan/10-observability-ops.md` §Metrics).
pub const EVAL_OVERALL_SCORE: &str = "kevin_eval_overall_score";
/// `kevin_eval_proposals_total` (`plan/10-observability-ops.md` §Metrics).
pub const EVAL_PROPOSALS: &str = "kevin_eval_proposals_total";

/// The model tag that marks an alias as judge-capable
/// (`plan/03-config-schema.md` `[models.*].tags`).
pub const JUDGE_TAG: &str = "judge";

/// What the evaluator needs from `[evaluation]`, `[roles]` and `[models]`.
#[derive(Debug, Clone)]
pub struct EvaluatorConfig {
    /// `[evaluation]`.
    pub evaluation: EvaluationCfg,
    /// `[roles]`.
    pub roles: Roles,
    /// `[models]`.
    pub models: BTreeMap<ModelAlias, ModelEntry>,
    /// `orchestrator.role_call_timeout`.
    pub timeout: Duration,
}

impl EvaluatorConfig {
    /// Reads the evaluator's slice of a resolved configuration.
    #[must_use]
    pub fn from_config(cfg: &KevinConfig) -> Self {
        Self {
            evaluation: cfg.evaluation.clone(),
            roles: cfg.roles.clone(),
            models: cfg.models.clone(),
            timeout: cfg.orchestrator.role_call_timeout,
        }
    }

    /// The effort `[roles.effort].judge` asks for.
    #[must_use]
    pub fn judge_effort(&self) -> Option<Effort> {
        self.roles.effort.get(&kevin_config::Role::Judge).copied()
    }
}

/// One subject to judge.
#[derive(Debug, Clone)]
pub struct EvaluationRequest {
    /// What is judged.
    pub subject: EvaluationSubject,
    /// Run the subject belongs to.
    pub run_id: RunId,
    /// Task kind (task subjects) — picks the rubric and the route outcome.
    pub task_kind: Option<TaskKind>,
    /// The attempt that produced the work.
    pub attempt: Option<OutcomeAttempt>,
    /// The route the work ran on; the judge never sees it.
    pub executor_route: Option<Route>,
    /// Whether the attempt succeeded (`false` is never judged, see §3.3).
    pub success: bool,
    /// Usage of the work being judged.
    pub usage: Usage,
    /// Memory scope lessons are stored in.
    pub scope: MemoryScope,
    /// What the judge is shown.
    pub evidence: Evidence,
}

impl EvaluationRequest {
    /// A run-level request.
    #[must_use]
    pub fn for_run(run_id: RunId, evidence: Evidence) -> Self {
        Self {
            subject: EvaluationSubject::Run(run_id),
            run_id,
            task_kind: None,
            attempt: None,
            executor_route: None,
            success: true,
            usage: evidence.usage,
            scope: MemoryScope::Global,
            evidence,
        }
    }

    /// A task-level request.
    #[must_use]
    pub fn for_task(
        run_id: RunId,
        task_id: kevin_domain::TaskId,
        kind: TaskKind,
        evidence: Evidence,
    ) -> Self {
        Self {
            subject: EvaluationSubject::Task(task_id),
            run_id,
            task_kind: Some(kind),
            attempt: None,
            executor_route: None,
            success: true,
            usage: evidence.usage,
            scope: MemoryScope::Global,
            evidence,
        }
    }

    /// Sets the attempt the outcome is keyed by.
    #[must_use]
    pub fn with_attempt(mut self, attempt: OutcomeAttempt) -> Self {
        self.attempt = Some(attempt);
        self
    }

    /// Sets the executor route (hidden from the judge, used for anti-gaming and
    /// for the route outcome).
    #[must_use]
    pub fn with_executor_route(mut self, route: Route) -> Self {
        self.executor_route = Some(route);
        self
    }

    /// Sets the memory scope.
    #[must_use]
    pub fn with_scope(mut self, scope: MemoryScope) -> Self {
        self.scope = scope;
        self
    }

    /// The attempt id, when there is one.
    #[must_use]
    pub fn attempt_id(&self) -> Option<AttemptId> {
        self.attempt.map(|a| a.attempt_id)
    }
}

/// What one `evaluate` call produced.
#[derive(Debug, Clone)]
pub struct EvaluationOutcome {
    /// The recorded evaluation.
    pub record: EvaluationRecord,
    /// The judge's raw answer (its own `overall` included, for the log).
    pub judge: JudgeOutput,
    /// What auto-apply did.
    pub applied: AutoApplyReport,
}

/// The evaluation service.
#[derive(Debug, Clone)]
pub struct Evaluator {
    cfg: EvaluatorConfig,
    runner: JudgeRunner,
    repo: Arc<dyn EvaluationRepo>,
    auto: AutoApply,
}

impl Evaluator {
    /// Builds an evaluator.
    #[must_use]
    pub fn new(
        cfg: EvaluatorConfig,
        workers: Arc<WorkerRegistry>,
        workspace: kevin_worker::Workspace,
        repo: Arc<dyn EvaluationRepo>,
        auto: AutoApply,
    ) -> Self {
        Self {
            cfg,
            runner: JudgeRunner::new(workers, workspace),
            repo,
            auto,
        }
    }

    /// Replaces the judge runner (cancellation token, other workspace).
    #[must_use]
    pub fn with_runner(mut self, runner: JudgeRunner) -> Self {
        self.runner = runner;
        self
    }

    /// The configuration slice in force.
    #[must_use]
    pub const fn config(&self) -> &EvaluatorConfig {
        &self.cfg
    }

    /// The repository (for `kevin proposals` and the API).
    #[must_use]
    pub fn repo(&self) -> &Arc<dyn EvaluationRepo> {
        &self.repo
    }

    /// `false` when configuration switches this subject's evaluation off.
    #[must_use]
    pub fn will_evaluate(&self, subject: EvaluationSubject) -> bool {
        self.skip_reason(subject).is_none()
    }

    /// Why this subject would be skipped, if it would.
    #[must_use]
    pub fn skip_reason(&self, subject: EvaluationSubject) -> Option<SkipReason> {
        if !self.cfg.evaluation.enabled {
            return Some(SkipReason::Disabled);
        }
        match subject {
            EvaluationSubject::Task(_) if !self.cfg.evaluation.evaluate_tasks => {
                Some(SkipReason::TasksDisabled)
            }
            _ => None,
        }
    }

    /// Judges `request`, records the evaluation and applies what policy allows.
    pub async fn evaluate(&self, request: EvaluationRequest) -> Result<EvaluationId> {
        self.evaluate_detailed(request).await.map(|o| o.record.id)
    }

    /// [`Evaluator::evaluate`], keeping the judge's answer and the auto-apply
    /// report (what the tests and `kevin eval` need).
    pub async fn evaluate_detailed(&self, request: EvaluationRequest) -> Result<EvaluationOutcome> {
        if let Some(reason) = self.skip_reason(request.subject) {
            return Err(reason.into());
        }
        let rubric = self.rubric_for(&request)?;
        let judge_route = self.judge_route(request.executor_route.as_ref());
        let ctx = JudgeContext::new(rubric.clone(), request.evidence.clone())
            .with_scrubber(self.scrubber(request.executor_route.as_ref()));

        let (judge, usage) = self
            .runner
            .call(
                &ctx,
                request.run_id,
                &judge_route,
                self.cfg.judge_effort(),
                self.cfg.timeout,
            )
            .await?;

        let overall = rubric.overall(&judge.score_pairs());
        let verdict = reconcile(judge.verdict, overall);
        if (overall - judge.overall).abs() > 0.05 {
            tracing::info!(
                judge_overall = judge.overall,
                recomputed = overall,
                rubric = %rubric.id,
                "judge overall disagreed with the weighted score; the weighted score wins"
            );
        }

        let record = EvaluationRecord {
            id: EvaluationId::new(),
            subject: request.subject,
            run_id: request.run_id,
            attempt_id: request.attempt_id(),
            rubric_id: rubric.id.clone(),
            judge_route,
            scores: scores(&judge)?,
            overall,
            verdict,
            lessons: judge.lessons.clone(),
            proposals: proposals(&judge),
            usage,
            created_at: Utc::now(),
        };
        self.repo.record(&record).await?;

        let applied = self
            .auto
            .apply(
                &record,
                &AutoApplyContext {
                    attempt: request.attempt,
                    task_kind: request.task_kind.clone(),
                    executor_alias: request.executor_route.as_ref().map(|r| r.model.clone()),
                    success: request.success,
                    usage: request.usage,
                    scope: request.scope.clone(),
                },
            )
            .await?;

        metrics::histogram!(
            EVAL_OVERALL_SCORE,
            "rubric" => record.rubric_id.clone(),
            "subject" => record.subject_type(),
        )
        .record(f64::from(record.overall));
        for proposal in &record.proposals {
            metrics::counter!(
                EVAL_PROPOSALS,
                "kind" => crate::repo::kind_str(proposal.kind),
                "status" => crate::repo::status_str(proposal.status),
            )
            .increment(1);
            tracing::info!(
                event = kevin_telemetry::events::eval::PROPOSAL_RAISED,
                proposal = %proposal.id,
                kind = crate::repo::kind_str(proposal.kind),
                "proposal raised"
            );
        }
        tracing::info!(
            event = kevin_telemetry::events::eval::RECORDED,
            evaluation = %record.id,
            run = %record.run_id,
            rubric = %record.rubric_id,
            overall = record.overall,
            verdict = record.verdict.as_str(),
            "evaluation recorded"
        );
        Ok(EvaluationOutcome {
            record,
            judge,
            applied,
        })
    }

    /// The rubric for a subject (`plan/06-memory-and-learning.md` §3.1).
    fn rubric_for(&self, request: &EvaluationRequest) -> Result<Rubric> {
        let configured = &self.cfg.evaluation.rubric;
        match request.subject {
            EvaluationSubject::Run(_) => Ok(Rubric::resolve(configured)?),
            EvaluationSubject::Task(_) => {
                Ok(rubric::for_kind(request.task_kind.as_ref(), configured)?)
            }
        }
    }

    /// The judge route: `[roles].judge` first, else any alias tagged `judge`;
    /// when two or more judge-capable aliases exist, one whose `worker + model`
    /// differs from the executor's (`plan/06-memory-and-learning.md` §3.2).
    #[must_use]
    pub fn judge_route(&self, executor: Option<&Route>) -> Route {
        let candidates = self.judge_candidates();
        let fallback = || {
            let alias = self.cfg.roles.judge.clone();
            let worker = self
                .cfg
                .models
                .get(&alias)
                .map_or(WorkerKind::Claude, |entry| entry.worker);
            Route::new(worker, alias)
        };
        let Some(first) = candidates.first() else {
            return fallback();
        };
        let excluded = executor.and_then(|route| self.identity(&route.model));
        if candidates.len() < 2 || excluded.is_none() {
            return self.route_of(first);
        }
        let excluded = excluded.expect("checked above");
        candidates
            .iter()
            .find(|alias| self.identity(alias).is_some_and(|id| id != excluded))
            .map_or_else(|| self.route_of(first), |alias| self.route_of(alias))
    }

    /// Judge-capable aliases: `[roles].judge` first, then every alias tagged
    /// `judge`, in config order, deduplicated.
    #[must_use]
    pub fn judge_candidates(&self) -> Vec<ModelAlias> {
        let mut out: Vec<ModelAlias> = Vec::new();
        if self.cfg.models.contains_key(&self.cfg.roles.judge) {
            out.push(self.cfg.roles.judge.clone());
        }
        for (alias, entry) in &self.cfg.models {
            if entry.tags.iter().any(|t| t == JUDGE_TAG) && !out.contains(alias) {
                out.push(alias.clone());
            }
        }
        out
    }

    /// `(worker, provider model id)` — what "a different model" means.
    fn identity(&self, alias: &ModelAlias) -> Option<(WorkerKind, String)> {
        self.cfg
            .models
            .get(alias)
            .map(|entry| (entry.worker, entry.model.clone()))
    }

    fn route_of(&self, alias: &ModelAlias) -> Route {
        let worker = self
            .cfg
            .models
            .get(alias)
            .map_or(WorkerKind::Claude, |entry| entry.worker);
        Route::new(worker, alias.clone())
    }

    /// Hides the executor's alias, provider model id and worker, plus every
    /// other configured alias, from the evidence.
    fn scrubber(&self, executor: Option<&Route>) -> Scrubber {
        let mut terms: Vec<String> = self
            .cfg
            .models
            .iter()
            .flat_map(|(alias, entry)| [alias.to_string(), entry.model.clone()])
            .collect();
        terms.extend(WorkerKind::ALL.iter().map(ToString::to_string));
        if let Some(route) = executor {
            terms.push(route.model.to_string());
            terms.push(route.worker.to_string());
        }
        Scrubber::new(terms)
    }
}

/// The stricter of the judge's verdict and the one the weighted score implies
/// (`plan/06-memory-and-learning.md` §3.2).
#[must_use]
pub fn reconcile(judge: Verdict, overall: f32) -> Verdict {
    judge.stricter(Verdict::from_overall(overall))
}

/// Judge scores → domain scores (validated).
fn scores(judge: &JudgeOutput) -> Result<Vec<RubricScore>> {
    judge
        .scores
        .iter()
        .map(|s| {
            RubricScore::new(&s.criterion, s.score, &s.rationale)
                .map_err(|e| EvaluatorError::Domain(e.into()))
        })
        .collect()
}

/// Judge proposals → inbox items (always `proposed`).
fn proposals(judge: &JudgeOutput) -> Vec<Proposal> {
    judge
        .proposals
        .iter()
        .map(|p| Proposal {
            id: ProposalId::new(),
            kind: p.kind,
            body: p.body.clone(),
            rationale: p.rationale.clone(),
            status: ProposalStatus::Proposed,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stricter_verdict_wins() {
        assert_eq!(reconcile(Verdict::Accept, 0.9), Verdict::Accept);
        // Generous judge, poor score.
        assert_eq!(reconcile(Verdict::Accept, 0.4), Verdict::Reject);
        // Strict judge, good score.
        assert_eq!(reconcile(Verdict::Reject, 0.95), Verdict::Reject);
        assert_eq!(
            reconcile(Verdict::AcceptWithFixes, 0.9),
            Verdict::AcceptWithFixes
        );
    }
}
