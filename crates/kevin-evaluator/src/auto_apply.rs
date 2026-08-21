//! The auto-apply policy executor (`plan/06-memory-and-learning.md` §3.4,
//! `plan/adr/0010-evaluation-auto-apply-policy.md`).
//!
//! | Part of the evaluation | `evaluation.auto_apply` contains | Action |
//! |---|---|---|
//! | `scores`/`verdict` | `routing` | `RecordRouteOutcome` for the attempt |
//! | `lessons[]` | `memory` | `StoreMemoryItem{kind: lesson}`, deduplicated against existing lessons at cosine ≥ 0.92 (supersede instead of duplicate) |
//! | `proposals[]` | — **never** | `eval.proposals` rows, status `proposed` |
//!
//! Prompt, config *and* routing proposals are all inbox items: nothing a judge
//! proposes is applied without a human. `evaluation.auto_apply` can only narrow
//! this list, never widen it.

use std::sync::Arc;

use chrono::Utc;
use kevin_config::AutoApply as AutoApplyPart;
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome};
use kevin_domain::{MemoryScope, MemorySource, TaskKind};
use kevin_memory::StoreRequest;

use crate::error::{EvaluatorError, Result};
use crate::memory_port::{LESSON_DEDUP_SIMILARITY, MemoryPort};
use crate::repo::EvaluationRecord;
use crate::router_port::{OutcomeAttempt, RouterPort};

/// Tag every lesson stored by an evaluation carries.
pub const LESSON_TAG: &str = "evaluation";
/// Tag added to lessons that came from a run-level evaluation
/// (`plan/06-memory-and-learning.md` §3.3).
pub const RUN_LESSON_TAG: &str = "run";

/// What auto-apply did, for logs and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoApplyReport {
    /// `RecordRouteOutcome` commands issued.
    pub route_outcomes: usize,
    /// Lessons stored as new items.
    pub lessons_stored: usize,
    /// Lessons that superseded a near-duplicate.
    pub lessons_superseded: usize,
    /// Proposals raised into the inbox (never applied).
    pub proposals_raised: usize,
}

/// Context an evaluation needs to apply its routing outcome.
#[derive(Debug, Clone)]
pub struct AutoApplyContext {
    /// The attempt that produced the work, when there was one.
    pub attempt: Option<OutcomeAttempt>,
    /// Task kind of the work.
    pub task_kind: Option<TaskKind>,
    /// The alias the work ran on (never shown to the judge).
    pub executor_alias: Option<kevin_domain::ModelAlias>,
    /// Whether the attempt itself succeeded.
    pub success: bool,
    /// Usage of the work being judged (cost/latency for the router).
    pub usage: kevin_domain::Usage,
    /// Memory scope lessons are stored in.
    pub scope: MemoryScope,
}

impl Default for AutoApplyContext {
    fn default() -> Self {
        Self {
            attempt: None,
            task_kind: None,
            executor_alias: None,
            success: true,
            usage: kevin_domain::Usage::ZERO,
            scope: MemoryScope::Global,
        }
    }
}

/// Applies the parts of an evaluation that `evaluation.auto_apply` allows.
#[derive(Debug, Clone, Default)]
pub struct AutoApply {
    parts: Vec<AutoApplyPart>,
    router: Option<Arc<dyn RouterPort>>,
    memory: Option<Arc<dyn MemoryPort>>,
}

impl AutoApply {
    /// A policy allowing `parts`.
    #[must_use]
    pub fn new(parts: impl IntoIterator<Item = AutoApplyPart>) -> Self {
        Self {
            parts: parts.into_iter().collect(),
            router: None,
            memory: None,
        }
    }

    /// A policy that changes nothing (proposals are still raised).
    #[must_use]
    pub fn none() -> Self {
        Self::new(Vec::new())
    }

    /// Wires the router.
    #[must_use]
    pub fn with_router(mut self, router: Arc<dyn RouterPort>) -> Self {
        self.router = Some(router);
        self
    }

    /// Wires memory.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryPort>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// `true` when `part` is allowed *and* wired.
    #[must_use]
    pub fn allows(&self, part: AutoApplyPart) -> bool {
        self.parts.contains(&part)
    }

    /// Applies `record`. Proposals are never applied here — they are already
    /// rows in `eval.proposals`, and this only counts them.
    pub async fn apply(
        &self,
        record: &EvaluationRecord,
        ctx: &AutoApplyContext,
    ) -> Result<AutoApplyReport> {
        let mut report = AutoApplyReport {
            proposals_raised: record.proposals.len(),
            ..AutoApplyReport::default()
        };
        if self.allows(AutoApplyPart::Routing) {
            report.route_outcomes = self.apply_routing(record, ctx).await?;
        }
        if self.allows(AutoApplyPart::Memory) {
            let (stored, superseded) = self.apply_memory(record, ctx).await?;
            report.lessons_stored = stored;
            report.lessons_superseded = superseded;
        }
        tracing::debug!(
            evaluation = %record.id,
            outcomes = report.route_outcomes,
            lessons = report.lessons_stored + report.lessons_superseded,
            proposals = report.proposals_raised,
            "auto-apply done"
        );
        Ok(report)
    }

    /// `RecordRouteOutcome` with `quality` = the recomputed overall.
    async fn apply_routing(
        &self,
        record: &EvaluationRecord,
        ctx: &AutoApplyContext,
    ) -> Result<usize> {
        let (Some(router), Some(alias), Some(kind)) = (
            self.router.as_ref(),
            ctx.executor_alias.as_ref(),
            ctx.task_kind.as_ref(),
        ) else {
            return Ok(0);
        };
        let cmd = RecordRouteOutcome {
            task_kind: kind.clone(),
            alias: alias.clone(),
            success: ctx.success,
            quality: Some(record.overall),
            cost_usd: ctx.usage.cost_usd,
            wall_ms: ctx.usage.wall_ms,
            failure_class: None,
            recorded_at: Utc::now(),
            prior: BetaPrior::default(),
        };
        router
            .record_outcome(cmd, ctx.attempt)
            .await
            .map_err(|e| EvaluatorError::AutoApply {
                part: "routing",
                message: e.to_string(),
            })?;
        Ok(1)
    }

    /// `StoreMemoryItem{kind: lesson}` per lesson, deduplicated at cosine
    /// ≥ [`LESSON_DEDUP_SIMILARITY`].
    async fn apply_memory(
        &self,
        record: &EvaluationRecord,
        ctx: &AutoApplyContext,
    ) -> Result<(usize, usize)> {
        let Some(memory) = self.memory.as_ref() else {
            return Ok((0, 0));
        };
        let mut stored = 0;
        let mut superseded = 0;
        for lesson in &record.lessons {
            let text = lesson.trim();
            if text.is_empty() {
                continue;
            }
            let req = lesson_request(text, record, ctx);
            let existing = memory
                .similar_lesson(text, LESSON_DEDUP_SIMILARITY)
                .await
                .map_err(|e| memory_err(&e))?;
            if let Some(old) = existing {
                memory
                    .supersede(old, req)
                    .await
                    .map_err(|e| memory_err(&e))?;
                superseded += 1;
            } else {
                memory.store(req).await.map_err(|e| memory_err(&e))?;
                stored += 1;
            }
        }
        Ok((stored, superseded))
    }
}

/// The `StoreMemoryItem` an evaluation lesson becomes.
fn lesson_request(text: &str, record: &EvaluationRecord, ctx: &AutoApplyContext) -> StoreRequest {
    let mut tags = vec![LESSON_TAG.to_owned(), record.rubric_id.clone()];
    match record.subject {
        kevin_domain::EvaluationSubject::Run(_) => tags.push(RUN_LESSON_TAG.to_owned()),
        kevin_domain::EvaluationSubject::Task(_) => {
            if let Some(kind) = &ctx.task_kind {
                tags.push(kind.to_string());
            }
        }
    }
    let source = MemorySource {
        run_id: Some(record.run_id),
        task_id: match record.subject {
            kevin_domain::EvaluationSubject::Task(id) => Some(id),
            kevin_domain::EvaluationSubject::Run(_) => None,
        },
        evaluation_id: Some(record.id),
        actor: crate::events::actor(),
    };
    StoreRequest::lesson(text)
        .with_tags(tags)
        .with_scope(ctx.scope.clone())
        .with_source(source)
}

fn memory_err(err: &crate::memory_port::MemoryPortError) -> EvaluatorError {
    EvaluatorError::AutoApply {
        part: "memory",
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_port::InMemoryLessons;
    use crate::router_port::InMemoryRouter;
    use kevin_domain::{
        AttemptId, EvaluationId, EvaluationSubject, ModelAlias, ProposalId, ProposalKind,
        ProposalStatus, Route, RunId, TaskId, Usage, Verdict, WorkerKind,
    };

    fn record(lessons: Vec<&str>, proposals: usize) -> EvaluationRecord {
        EvaluationRecord {
            id: EvaluationId::new(),
            subject: EvaluationSubject::Task(TaskId::new()),
            run_id: RunId::new(),
            attempt_id: Some(AttemptId::new()),
            rubric_id: "code".to_owned(),
            judge_route: Route::new(WorkerKind::Fake, ModelAlias::new("fake").unwrap()),
            scores: Vec::new(),
            overall: 0.82,
            verdict: Verdict::Accept,
            lessons: lessons.into_iter().map(ToOwned::to_owned).collect(),
            proposals: (0..proposals)
                .map(|i| kevin_domain::Proposal {
                    id: ProposalId::new(),
                    kind: ProposalKind::Prompt,
                    body: format!("body {i}"),
                    rationale: String::new(),
                    status: ProposalStatus::Proposed,
                })
                .collect(),
            usage: Usage::ZERO,
            created_at: Utc::now(),
        }
    }

    fn ctx() -> AutoApplyContext {
        AutoApplyContext {
            attempt: Some(OutcomeAttempt::new(
                RunId::new(),
                TaskId::new(),
                AttemptId::new(),
            )),
            task_kind: Some(TaskKind::Implement),
            executor_alias: Some(ModelAlias::new("sonnet5-claude").unwrap()),
            ..AutoApplyContext::default()
        }
    }

    #[tokio::test]
    async fn routing_and_memory_are_applied_when_allowed() {
        let router = Arc::new(InMemoryRouter::new());
        let memory = Arc::new(InMemoryLessons::new());
        let policy = AutoApply::new([AutoApplyPart::Routing, AutoApplyPart::Memory])
            .with_router(router.clone())
            .with_memory(memory.clone());
        let rec = record(vec!["run the tests first", "run the tests first"], 2);
        let report = policy.apply(&rec, &ctx()).await.unwrap();

        assert_eq!(report.route_outcomes, 1);
        assert_eq!(report.lessons_stored, 1);
        assert_eq!(report.lessons_superseded, 1);
        assert_eq!(report.proposals_raised, 2);
        assert_eq!(memory.lessons().len(), 1);
        let outcomes = router.outcomes();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].quality, Some(0.82));
        assert_eq!(outcomes[0].task_kind, TaskKind::Implement);
    }

    #[tokio::test]
    async fn a_narrowed_policy_applies_nothing_it_was_not_given() {
        let router = Arc::new(InMemoryRouter::new());
        let memory = Arc::new(InMemoryLessons::new());
        let policy = AutoApply::new([AutoApplyPart::Memory])
            .with_router(router.clone())
            .with_memory(memory.clone());
        let report = policy
            .apply(&record(vec!["a lesson"], 1), &ctx())
            .await
            .unwrap();
        assert_eq!(report.route_outcomes, 0);
        assert!(router.is_empty());
        assert_eq!(report.lessons_stored, 1);

        let none = AutoApply::none()
            .with_router(router.clone())
            .with_memory(memory);
        let report = none
            .apply(&record(vec!["b lesson"], 1), &ctx())
            .await
            .unwrap();
        assert_eq!(report.route_outcomes, 0);
        assert_eq!(report.lessons_stored, 0);
        assert_eq!(report.proposals_raised, 1);
    }
}
