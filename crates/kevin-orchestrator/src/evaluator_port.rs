//! [`EvaluationRunner`] — the production [`EvaluatorPort`] over WS-19's
//! [`Evaluator`].
//!
//! The saga asks one question ("judge this finished run and its tasks"); the
//! evaluator asks for [`Evidence`], which only the `orch` read models can
//! supply. This adapter is that translation and nothing else: it reads
//! `orch.run_overview`, `orch.task_board` and `orch.artifacts`, judges the
//! succeeded tasks first (when `evaluation.evaluate_tasks` is on) so their
//! verdicts become run-level evidence, then judges the run.
//!
//! Failures are **transient** port errors on purpose: `plan/05-orchestration.md`
//! §3.7 completes the run with `evaluation: skipped` rather than failing it
//! because a judge was unavailable.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kevin_domain::run::RunEvaluation;
use kevin_domain::{EvaluationSubject, Route, RunId, TaskId, Usage};
use kevin_evaluator::{
    ArtifactLine, EvaluationRequest, Evaluator, EvaluatorError, Evidence, OutcomeAttempt,
    TaskVerdict,
};

use crate::ports::{EvaluatorPort, PortError, PortResult};
use crate::projections::{ArtifactRow, ReadModels, RunOverviewRow, TaskBoardRow};

/// How long the adapter waits for `orch.run_overview` to carry the run it must
/// judge. The saga reacts to `run.integrated` off the bus, which the projection
/// runner consumes concurrently, so the row can be a few events behind.
const EVIDENCE_WAIT: Duration = Duration::from_secs(5);
/// Poll interval of that wait.
const EVIDENCE_POLL: Duration = Duration::from_millis(50);

/// Judges finished runs through `kevin-evaluator`.
#[derive(Debug, Clone)]
pub struct EvaluationRunner {
    evaluator: Arc<Evaluator>,
    reads: ReadModels,
}

impl EvaluationRunner {
    /// Wires `evaluator` to the `orch` read models it draws evidence from.
    #[must_use]
    pub const fn new(evaluator: Arc<Evaluator>, reads: ReadModels) -> Self {
        Self { evaluator, reads }
    }

    /// The evaluator behind the port (`kevin eval rerun`, the API).
    #[must_use]
    pub fn evaluator(&self) -> &Arc<Evaluator> {
        &self.evaluator
    }

    /// The run's overview row, waiting up to [`EVIDENCE_WAIT`] for the
    /// projection to catch up with the events the saga has already seen.
    async fn await_run(&self, run_id: RunId) -> PortResult<Option<RunOverviewRow>> {
        let deadline = tokio::time::Instant::now() + EVIDENCE_WAIT;
        loop {
            let row = self
                .reads
                .run(run_id.as_uuid())
                .await
                .map_err(|e| PortError::transient("evaluator", e.to_string()))?;
            if row.is_some() || tokio::time::Instant::now() >= deadline {
                return Ok(row);
            }
            tokio::time::sleep(EVIDENCE_POLL).await;
        }
    }

    /// Judges every succeeded task and returns their verdicts, which become
    /// run-level evidence. A task that cannot be judged is logged and skipped:
    /// the run-level verdict is what the saga waits for.
    async fn evaluate_tasks(&self, run_id: RunId, task_ids: &[TaskId]) -> Vec<TaskVerdict> {
        let mut verdicts = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            let Ok(Some(task)) = self.reads.task(task_id.as_uuid()).await else {
                continue;
            };
            let mut verdict = TaskVerdict {
                title: task.title.clone(),
                kind: task.kind.clone(),
                verdict: None,
                overall: None,
            };
            if task.status == "succeeded"
                && self
                    .evaluator
                    .will_evaluate(EvaluationSubject::Task(*task_id))
            {
                match self.judge_task(run_id, *task_id, &task).await {
                    Ok(Some((overall, judged))) => {
                        verdict.overall = Some(overall);
                        verdict.verdict = Some(judged);
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::warn!(task = %task_id, error = %err, "task evaluation failed");
                    }
                }
            }
            verdicts.push(verdict);
        }
        verdicts
    }

    async fn judge_task(
        &self,
        run_id: RunId,
        task_id: TaskId,
        task: &TaskBoardRow,
    ) -> Result<Option<(f32, kevin_domain::Verdict)>, EvaluatorError> {
        let Ok(kind) = task.kind.parse::<kevin_domain::TaskKind>() else {
            return Ok(None);
        };
        let artifacts = self
            .reads
            .artifacts_of_task(task_id.as_uuid())
            .await
            .unwrap_or_default();
        let mut evidence = Evidence::new(format!("{}\n\n{}", task.title, task.instructions))
            .with_acceptance_criteria(strings(&task.acceptance_criteria))
            .with_artifacts(artifact_lines(&artifacts))
            .with_usage(usage(&task.usage));
        if let Some(summary) = &task.summary {
            evidence = evidence.with_transcript_summary(summary.clone());
        }

        let mut request = EvaluationRequest::for_task(run_id, task_id, kind, evidence);
        if let Some(route) = task.route.clone().and_then(parse_route) {
            request = request.with_executor_route(route);
        }
        if let Some(attempt_id) = last_attempt(task) {
            request = request.with_attempt(OutcomeAttempt::new(run_id, task_id, attempt_id));
        }
        match self.evaluator.evaluate_detailed(request).await {
            Ok(outcome) => Ok(Some((outcome.record.overall, outcome.record.verdict))),
            Err(err) if err.is_skipped() => Ok(None),
            Err(err) => Err(err),
        }
    }

    async fn run_evidence(&self, run: &RunOverviewRow, verdicts: Vec<TaskVerdict>) -> Evidence {
        let artifacts = self
            .reads
            .artifacts_of_run(run.run_id)
            .await
            .unwrap_or_default();
        let mut evidence = Evidence::new(run.goal_text.clone())
            .with_artifacts(artifact_lines(&artifacts))
            .with_usage(usage(&run.usage));
        evidence.success_criteria = run
            .understanding
            .as_ref()
            .map(|u| strings(&u["success_criteria"]))
            .unwrap_or_default();
        evidence
            .acceptance_criteria
            .clone_from(&evidence.success_criteria);
        evidence.plan = run.plan.as_ref().map(plan_outline);
        evidence.integration = run.summary.clone();
        evidence.task_verdicts = verdicts;
        evidence
    }
}

#[async_trait]
impl EvaluatorPort for EvaluationRunner {
    async fn evaluate_run(
        &self,
        run_id: RunId,
        task_ids: &[TaskId],
    ) -> PortResult<Option<RunEvaluation>> {
        if !self.evaluator.will_evaluate(EvaluationSubject::Run(run_id)) {
            return Ok(None);
        }
        let Some(run) = self.await_run(run_id).await? else {
            tracing::warn!(run = %run_id, "no read model for the run; evaluation skipped");
            return Ok(None);
        };
        let verdicts = self.evaluate_tasks(run_id, task_ids).await;
        let evidence = self.run_evidence(&run, verdicts).await;
        match self
            .evaluator
            .evaluate_detailed(EvaluationRequest::for_run(run_id, evidence))
            .await
        {
            Ok(outcome) => Ok(Some(RunEvaluation {
                evaluation_id: outcome.record.id,
                overall: outcome.record.overall,
                verdict: outcome.record.verdict,
            })),
            Err(err) if err.is_skipped() => Ok(None),
            Err(err) => Err(PortError::transient("evaluator", err.to_string())),
        }
    }
}

/// The JSON string array of a read-model column.
fn strings(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// `Usage` as stored on the read models; a shape change degrades to zero
/// rather than failing an evaluation.
fn usage(value: &serde_json::Value) -> Usage {
    serde_json::from_value(value.clone()).unwrap_or(Usage::ZERO)
}

fn parse_route(value: serde_json::Value) -> Option<Route> {
    serde_json::from_value(value).ok()
}

/// The attempt id of the last attempt, which is the one that produced the work.
fn last_attempt(task: &TaskBoardRow) -> Option<kevin_domain::AttemptId> {
    let last = task.attempts.as_array()?.last()?;
    last.get("id")
        .or_else(|| last.get("attempt_id"))
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.parse().ok())
}

fn artifact_lines(rows: &[ArtifactRow]) -> Vec<ArtifactLine> {
    rows.iter()
        .map(|row| ArtifactLine {
            kind: row.kind.clone(),
            uri: row.uri.clone(),
            description: None,
        })
        .collect()
}

/// A compact rendering of the approved plan for the judge.
fn plan_outline(plan: &serde_json::Value) -> String {
    let tasks = plan
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .map(|tasks| {
            tasks
                .iter()
                .map(|task| {
                    format!(
                        "- {}: {}",
                        task.get("kind")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("task"),
                        task.get("title")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("(untitled)")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match plan.get("rationale").and_then(serde_json::Value::as_str) {
        Some(rationale) if !rationale.is_empty() => format!("{rationale}\n{tasks}"),
        _ => tasks,
    }
}
