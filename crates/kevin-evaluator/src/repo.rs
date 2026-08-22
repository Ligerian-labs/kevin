//! Persistence of the `Evaluation` aggregate and its two projections
//! (`plan/06-memory-and-learning.md` §3.5).
//!
//! `eval.evaluations` and `eval.proposals` are projections of `evaluation.*`
//! (rebuildable); the event stream stays the source of truth. Everything the
//! evaluator and `kevin proposals` need goes through [`EvaluationRepo`], so the
//! judge, the auto-apply policy and the CLI are all testable without Postgres
//! ([`InMemoryEvaluationRepo`]).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kevin_domain::aggregate::Aggregate as _;
use kevin_domain::evaluation::{
    AcceptProposal, Evaluation, EvaluationCommand, ProposalDraft, RecordEvaluation, RejectProposal,
};
use kevin_domain::{
    AttemptId, EvaluationEvent, EvaluationId, EvaluationSubject, ModelAlias, Proposal, ProposalId,
    ProposalKind, ProposalStatus, Route, RubricScore, RunId, Usage, Verdict, WorkerKind,
};
use kevin_store::event_store::{EventStore, NewEvent};
use kevin_store::{PgPool, StoreError};
use serde_json::Value;
use sqlx::Row as _;
use uuid::Uuid;

use crate::error::{EvaluatorError, Result};
use crate::events;

/// `subject_type` of a run evaluation.
pub const SUBJECT_RUN: &str = "run";
/// `subject_type` of a task evaluation.
pub const SUBJECT_TASK: &str = "task";

/// One `eval.evaluations` row — everything `evaluation.recorded` carries plus
/// the run/attempt it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationRecord {
    /// Evaluation id.
    pub id: EvaluationId,
    /// What was judged.
    pub subject: EvaluationSubject,
    /// Run the subject belongs to.
    pub run_id: RunId,
    /// Attempt the subject belongs to (task evaluations).
    pub attempt_id: Option<AttemptId>,
    /// Rubric used.
    pub rubric_id: String,
    /// Route the judge ran on.
    pub judge_route: Route,
    /// Per-criterion scores.
    pub scores: Vec<RubricScore>,
    /// Overall, recomputed from the rubric weights.
    pub overall: f32,
    /// Verdict after reconciliation.
    pub verdict: Verdict,
    /// Lessons.
    pub lessons: Vec<String>,
    /// Proposals, always `proposed` at record time.
    pub proposals: Vec<Proposal>,
    /// Usage of the judge call.
    pub usage: Usage,
    /// When it was recorded.
    pub created_at: DateTime<Utc>,
}

impl EvaluationRecord {
    /// `run` or `task`.
    #[must_use]
    pub const fn subject_type(&self) -> &'static str {
        match self.subject {
            EvaluationSubject::Run(_) => SUBJECT_RUN,
            EvaluationSubject::Task(_) => SUBJECT_TASK,
        }
    }

    /// The subject's uuid.
    #[must_use]
    pub fn subject_id(&self) -> Uuid {
        match self.subject {
            EvaluationSubject::Run(id) => id.as_uuid(),
            EvaluationSubject::Task(id) => id.as_uuid(),
        }
    }

    /// The `RecordEvaluation` command this record stands for.
    #[must_use]
    pub fn command(&self) -> RecordEvaluation {
        RecordEvaluation {
            evaluation_id: self.id,
            subject: self.subject,
            rubric_id: self.rubric_id.clone(),
            judge_route: self.judge_route.clone(),
            scores: self.scores.clone(),
            overall: self.overall,
            verdict: self.verdict,
            lessons: self.lessons.clone(),
            proposals: self
                .proposals
                .iter()
                .map(|p| ProposalDraft {
                    id: p.id,
                    kind: p.kind,
                    body: p.body.clone(),
                    rationale: p.rationale.clone(),
                })
                .collect(),
            usage: self.usage,
        }
    }

    /// The `eval.proposals` rows this record produces.
    #[must_use]
    pub fn proposal_rows(&self) -> Vec<ProposalRow> {
        self.proposals
            .iter()
            .map(|p| ProposalRow {
                id: p.id,
                evaluation_id: self.id,
                run_id: self.run_id,
                kind: p.kind,
                body: p.body.clone(),
                rationale: p.rationale.clone(),
                status: p.status,
                decided_by: None,
                decided_at: None,
                created_at: self.created_at,
            })
            .collect()
    }
}

/// One `eval.proposals` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalRow {
    /// Proposal id.
    pub id: ProposalId,
    /// The evaluation that raised it.
    pub evaluation_id: EvaluationId,
    /// The run it came from.
    pub run_id: RunId,
    /// What it changes.
    pub kind: ProposalKind,
    /// The proposed change.
    pub body: String,
    /// Why.
    pub rationale: String,
    /// Inbox status.
    pub status: ProposalStatus,
    /// Who decided.
    pub decided_by: Option<String>,
    /// When it was decided.
    pub decided_at: Option<DateTime<Utc>>,
    /// When it was raised.
    pub created_at: DateTime<Utc>,
}

/// A human decision on a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// `evaluation.proposal_accepted`.
    Accept,
    /// `evaluation.proposal_rejected`.
    Reject,
}

impl Decision {
    /// The resulting status.
    #[must_use]
    pub const fn status(self) -> ProposalStatus {
        match self {
            Decision::Accept => ProposalStatus::Accepted,
            Decision::Reject => ProposalStatus::Rejected,
        }
    }

    /// The command this decision issues.
    #[must_use]
    pub fn command(self, proposal_id: ProposalId, by: &str) -> EvaluationCommand {
        match self {
            Decision::Accept => EvaluationCommand::AcceptProposal(AcceptProposal {
                proposal_id,
                by: by.to_owned(),
            }),
            Decision::Reject => EvaluationCommand::RejectProposal(RejectProposal {
                proposal_id,
                by: by.to_owned(),
            }),
        }
    }
}

/// Persistence of evaluations and proposals.
#[async_trait]
pub trait EvaluationRepo: Send + Sync + std::fmt::Debug {
    /// Appends `evaluation.recorded` and writes both projection rows.
    async fn record(&self, record: &EvaluationRecord) -> Result<()>;

    /// One evaluation by id.
    async fn evaluation(&self, id: EvaluationId) -> Result<Option<EvaluationRecord>>;

    /// Evaluations of a subject, newest first.
    async fn evaluations_of(&self, subject: EvaluationSubject) -> Result<Vec<EvaluationRecord>>;

    /// One proposal by id.
    async fn proposal(&self, id: ProposalId) -> Result<Option<ProposalRow>>;

    /// The inbox: proposals with `status`, newest first.
    async fn proposals(
        &self,
        status: Option<ProposalStatus>,
        limit: usize,
    ) -> Result<Vec<ProposalRow>>;

    /// Runs a human decision through the aggregate, appends its event and
    /// updates the projection row.
    async fn decide(
        &self,
        proposal_id: ProposalId,
        decision: Decision,
        by: &str,
    ) -> Result<ProposalRow>;
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

/// The `eval` schema implementation.
#[derive(Clone)]
pub struct PgEvaluationRepo {
    pool: PgPool,
    events: Arc<dyn EventStore>,
}

impl std::fmt::Debug for PgEvaluationRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgEvaluationRepo").finish_non_exhaustive()
    }
}

impl PgEvaluationRepo {
    /// Builds a repository over `pool`, appending events through `events`.
    #[must_use]
    pub fn new(pool: PgPool, events: Arc<dyn EventStore>) -> Self {
        Self { pool, events }
    }

    /// The pool (for `kevin db` style tooling and tests).
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Rehydrates the aggregate from its event stream.
    async fn load(&self, id: EvaluationId) -> Result<(Evaluation, u64)> {
        let stored = self
            .events
            .load_stream(&events::stream(id), 0)
            .await
            .map_err(|e| store_err(&e))?;
        if stored.is_empty() {
            return Err(EvaluatorError::EvaluationNotFound(id));
        }
        let mut aggregate = Evaluation::default();
        let mut version = 0u64;
        for event in stored {
            let payload: EvaluationEvent =
                serde_json::from_value(event.envelope.payload).map_err(EvaluatorError::store)?;
            aggregate.apply(&payload);
            version = version.saturating_add(1);
        }
        Ok((aggregate, version))
    }

    async fn append(
        &self,
        run_id: RunId,
        id: EvaluationId,
        expected: u64,
        event: &EvaluationEvent,
    ) -> Result<()> {
        let new_event: NewEvent =
            events::new_event(run_id, event, events::actor()).map_err(EvaluatorError::store)?;
        self.events
            .append(
                &events::stream(id),
                expected,
                std::slice::from_ref(&new_event),
            )
            .await
            .map_err(|e| store_err(&e))?;
        Ok(())
    }
}

/// `StoreError` → [`EvaluatorError::Store`].
fn store_err(err: &StoreError) -> EvaluatorError {
    EvaluatorError::Store(err.to_string())
}

#[async_trait]
impl EvaluationRepo for PgEvaluationRepo {
    async fn record(&self, record: &EvaluationRecord) -> Result<()> {
        // The aggregate validates before anything is written.
        let aggregate = Evaluation::default();
        let produced = aggregate.handle(&EvaluationCommand::Record(record.command()))?;
        let [event] = produced.as_slice() else {
            return Err(EvaluatorError::store("record_evaluation produced no event"));
        };
        self.append(record.run_id, record.id, 0, event).await?;

        let mut tx = self.pool.begin().await.map_err(EvaluatorError::store)?;
        sqlx::query(
            "INSERT INTO eval.evaluations (id, subject_type, subject_id, run_id, attempt_id, \
             rubric_id, judge_alias, judge_worker, scores, overall, verdict, lessons, usage, created_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT (id) DO NOTHING",
        )
        .bind(record.id.as_uuid())
        .bind(record.subject_type())
        .bind(record.subject_id())
        .bind(record.run_id.as_uuid())
        .bind(record.attempt_id.map(|id| id.as_uuid()))
        .bind(&record.rubric_id)
        .bind(record.judge_route.model.to_string())
        .bind(record.judge_route.worker.to_string())
        .bind(serde_json::to_value(&record.scores).map_err(EvaluatorError::store)?)
        .bind(record.overall)
        .bind(record.verdict.as_str())
        .bind(&record.lessons)
        .bind(serde_json::to_value(record.usage).map_err(EvaluatorError::store)?)
        .bind(record.created_at)
        .execute(&mut *tx)
        .await
        .map_err(EvaluatorError::store)?;

        for row in record.proposal_rows() {
            sqlx::query(
                "INSERT INTO eval.proposals (id, evaluation_id, run_id, kind, body, rationale, status, created_at) \
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING",
            )
            .bind(row.id.as_uuid())
            .bind(row.evaluation_id.as_uuid())
            .bind(row.run_id.as_uuid())
            .bind(kind_str(row.kind))
            .bind(&row.body)
            .bind(&row.rationale)
            .bind(status_str(row.status))
            .bind(row.created_at)
            .execute(&mut *tx)
            .await
            .map_err(EvaluatorError::store)?;
        }
        tx.commit().await.map_err(EvaluatorError::store)?;
        Ok(())
    }

    async fn evaluation(&self, id: EvaluationId) -> Result<Option<EvaluationRecord>> {
        let row = sqlx::query(EVALUATION_SELECT_BY_ID)
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(EvaluatorError::store)?;
        let Some(row) = row else { return Ok(None) };
        let proposals = self.proposals_of(id).await?;
        Ok(Some(evaluation_from_row(&row, &proposals)?))
    }

    async fn evaluations_of(&self, subject: EvaluationSubject) -> Result<Vec<EvaluationRecord>> {
        let (subject_type, subject_id) = match subject {
            EvaluationSubject::Run(id) => (SUBJECT_RUN, id.as_uuid()),
            EvaluationSubject::Task(id) => (SUBJECT_TASK, id.as_uuid()),
        };
        let rows = sqlx::query(EVALUATION_SELECT_BY_SUBJECT)
            .bind(subject_type)
            .bind(subject_id)
            .fetch_all(&self.pool)
            .await
            .map_err(EvaluatorError::store)?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let id = EvaluationId::from_uuid(row.try_get("id").map_err(EvaluatorError::store)?);
            let proposals = self.proposals_of(id).await?;
            out.push(evaluation_from_row(row, &proposals)?);
        }
        Ok(out)
    }

    async fn proposal(&self, id: ProposalId) -> Result<Option<ProposalRow>> {
        let row = sqlx::query(PROPOSAL_SELECT_BY_ID)
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(EvaluatorError::store)?;
        row.as_ref().map(proposal_from_row).transpose()
    }

    async fn proposals(
        &self,
        status: Option<ProposalStatus>,
        limit: usize,
    ) -> Result<Vec<ProposalRow>> {
        let rows = sqlx::query(PROPOSAL_SELECT_INBOX)
            .bind(status.map(status_str))
            .bind(i64::try_from(limit).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await
            .map_err(EvaluatorError::store)?;
        rows.iter().map(proposal_from_row).collect()
    }

    async fn decide(
        &self,
        proposal_id: ProposalId,
        decision: Decision,
        by: &str,
    ) -> Result<ProposalRow> {
        let row = self
            .proposal(proposal_id)
            .await?
            .ok_or(EvaluatorError::ProposalNotFound(proposal_id))?;
        let (aggregate, version) = self.load(row.evaluation_id).await?;
        let produced = aggregate.handle(&decision.command(proposal_id, by))?;
        let [event] = produced.as_slice() else {
            return Err(EvaluatorError::store("decision produced no event"));
        };
        self.append(row.run_id, row.evaluation_id, version, event)
            .await?;
        let decided_at = Utc::now();
        sqlx::query(
            "UPDATE eval.proposals SET status = $2, decided_by = $3, decided_at = $4 WHERE id = $1",
        )
        .bind(proposal_id.as_uuid())
        .bind(status_str(decision.status()))
        .bind(by)
        .bind(decided_at)
        .execute(&self.pool)
        .await
        .map_err(EvaluatorError::store)?;
        Ok(ProposalRow {
            status: decision.status(),
            decided_by: Some(by.to_owned()),
            decided_at: Some(decided_at),
            ..row
        })
    }
}

impl PgEvaluationRepo {
    /// Proposal rows of one evaluation, in creation order.
    async fn proposals_of(&self, id: EvaluationId) -> Result<Vec<ProposalRow>> {
        let rows = sqlx::query(PROPOSAL_SELECT_BY_EVALUATION)
            .bind(id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(EvaluatorError::store)?;
        rows.iter().map(proposal_from_row).collect()
    }
}

/// Columns of `eval.evaluations`, in DDL order.
macro_rules! evaluation_columns {
    () => {
        "id, subject_type, subject_id, run_id, attempt_id, rubric_id, judge_alias, judge_worker, \
         scores, overall, verdict, lessons, usage, created_at"
    };
}

const EVALUATION_SELECT_BY_ID: &str = concat!(
    "SELECT ",
    evaluation_columns!(),
    " FROM eval.evaluations WHERE id = $1"
);
const EVALUATION_SELECT_BY_SUBJECT: &str = concat!(
    "SELECT ",
    evaluation_columns!(),
    " FROM eval.evaluations WHERE subject_type = $1 AND subject_id = $2 ORDER BY created_at DESC"
);
/// Columns of `eval.proposals`, in DDL order.
macro_rules! proposal_columns {
    () => {
        "id, evaluation_id, run_id, kind, body, rationale, status, decided_by, decided_at, created_at"
    };
}

const PROPOSAL_SELECT_BY_ID: &str = concat!(
    "SELECT ",
    proposal_columns!(),
    " FROM eval.proposals WHERE id = $1"
);
const PROPOSAL_SELECT_INBOX: &str = concat!(
    "SELECT ",
    proposal_columns!(),
    " FROM eval.proposals WHERE ($1::text IS NULL OR status = $1) \
     ORDER BY created_at DESC, id DESC LIMIT $2"
);
const PROPOSAL_SELECT_BY_EVALUATION: &str = concat!(
    "SELECT ",
    proposal_columns!(),
    " FROM eval.proposals WHERE evaluation_id = $1 ORDER BY created_at, id"
);

/// `eval.evaluations` row → [`EvaluationRecord`]. The projection stores the
/// judge's alias and worker (the DDL in `plan/06` §3.5); the effort lives on
/// the event, so a route read back here never carries one.
fn evaluation_from_row(
    row: &sqlx::postgres::PgRow,
    proposals: &[ProposalRow],
) -> Result<EvaluationRecord> {
    let subject_type: String = row.try_get("subject_type").map_err(EvaluatorError::store)?;
    let subject_id: Uuid = row.try_get("subject_id").map_err(EvaluatorError::store)?;
    let subject = match subject_type.as_str() {
        SUBJECT_RUN => EvaluationSubject::Run(RunId::from_uuid(subject_id)),
        _ => EvaluationSubject::Task(kevin_domain::TaskId::from_uuid(subject_id)),
    };
    let alias: String = row.try_get("judge_alias").map_err(EvaluatorError::store)?;
    let worker: String = row.try_get("judge_worker").map_err(EvaluatorError::store)?;
    let scores: Value = row.try_get("scores").map_err(EvaluatorError::store)?;
    let usage: Value = row.try_get("usage").map_err(EvaluatorError::store)?;
    let verdict: String = row.try_get("verdict").map_err(EvaluatorError::store)?;
    let attempt: Option<Uuid> = row.try_get("attempt_id").map_err(EvaluatorError::store)?;
    Ok(EvaluationRecord {
        id: EvaluationId::from_uuid(row.try_get("id").map_err(EvaluatorError::store)?),
        subject,
        run_id: RunId::from_uuid(row.try_get("run_id").map_err(EvaluatorError::store)?),
        attempt_id: attempt.map(AttemptId::from_uuid),
        rubric_id: row.try_get("rubric_id").map_err(EvaluatorError::store)?,
        judge_route: Route::new(
            worker
                .parse::<WorkerKind>()
                .map_err(EvaluatorError::store)?,
            ModelAlias::new(alias).map_err(EvaluatorError::store)?,
        ),
        scores: serde_json::from_value(scores).map_err(EvaluatorError::store)?,
        overall: row.try_get("overall").map_err(EvaluatorError::store)?,
        verdict: parse_verdict(&verdict)?,
        lessons: row.try_get("lessons").map_err(EvaluatorError::store)?,
        proposals: proposals.iter().map(proposal_value).collect(),
        usage: serde_json::from_value(usage).map_err(EvaluatorError::store)?,
        created_at: row.try_get("created_at").map_err(EvaluatorError::store)?,
    })
}

fn proposal_from_row(row: &sqlx::postgres::PgRow) -> Result<ProposalRow> {
    let kind: String = row.try_get("kind").map_err(EvaluatorError::store)?;
    let status: String = row.try_get("status").map_err(EvaluatorError::store)?;
    Ok(ProposalRow {
        id: ProposalId::from_uuid(row.try_get("id").map_err(EvaluatorError::store)?),
        evaluation_id: EvaluationId::from_uuid(
            row.try_get("evaluation_id")
                .map_err(EvaluatorError::store)?,
        ),
        run_id: RunId::from_uuid(row.try_get("run_id").map_err(EvaluatorError::store)?),
        kind: parse_kind(&kind)?,
        body: row.try_get("body").map_err(EvaluatorError::store)?,
        rationale: row.try_get("rationale").map_err(EvaluatorError::store)?,
        status: parse_status(&status)?,
        decided_by: row.try_get("decided_by").map_err(EvaluatorError::store)?,
        decided_at: row.try_get("decided_at").map_err(EvaluatorError::store)?,
        created_at: row.try_get("created_at").map_err(EvaluatorError::store)?,
    })
}

fn proposal_value(row: &ProposalRow) -> Proposal {
    Proposal {
        id: row.id,
        kind: row.kind,
        body: row.body.clone(),
        rationale: row.rationale.clone(),
        status: row.status,
    }
}

/// `ProposalKind` → the `eval.proposals.kind` check-constraint value.
#[must_use]
pub const fn kind_str(kind: ProposalKind) -> &'static str {
    match kind {
        ProposalKind::Prompt => "prompt",
        ProposalKind::Config => "config",
        ProposalKind::Routing => "routing",
    }
}

/// `ProposalStatus` → the `eval.proposals.status` check-constraint value.
#[must_use]
pub const fn status_str(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Proposed => "proposed",
        ProposalStatus::Accepted => "accepted",
        ProposalStatus::Rejected => "rejected",
    }
}

/// Parses a `eval.proposals.kind` value.
pub fn parse_kind(kind: &str) -> Result<ProposalKind> {
    match kind {
        "prompt" => Ok(ProposalKind::Prompt),
        "config" => Ok(ProposalKind::Config),
        "routing" => Ok(ProposalKind::Routing),
        other => Err(EvaluatorError::store(format!(
            "unknown proposal kind `{other}`"
        ))),
    }
}

/// Parses a `eval.proposals.status` value.
pub fn parse_status(status: &str) -> Result<ProposalStatus> {
    match status {
        "proposed" => Ok(ProposalStatus::Proposed),
        "accepted" => Ok(ProposalStatus::Accepted),
        "rejected" => Ok(ProposalStatus::Rejected),
        other => Err(EvaluatorError::store(format!(
            "unknown proposal status `{other}`"
        ))),
    }
}

/// Parses a `eval.evaluations.verdict` value.
pub fn parse_verdict(verdict: &str) -> Result<Verdict> {
    Verdict::ALL
        .into_iter()
        .find(|v| v.as_str() == verdict)
        .ok_or_else(|| EvaluatorError::store(format!("unknown verdict `{verdict}`")))
}

// ---------------------------------------------------------------------------
// In-memory
// ---------------------------------------------------------------------------

/// An [`EvaluationRepo`] that keeps everything in memory, aggregate included.
/// Used by the acceptance tests that do not need Postgres.
#[derive(Debug, Default)]
pub struct InMemoryEvaluationRepo {
    state: Mutex<InMemoryState>,
}

#[derive(Debug, Default)]
struct InMemoryState {
    evaluations: BTreeMap<Uuid, (EvaluationRecord, Evaluation)>,
    order: Vec<EvaluationId>,
    events: Vec<EvaluationEvent>,
}

impl InMemoryEvaluationRepo {
    /// An empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event it recorded, in order.
    #[must_use]
    pub fn events(&self) -> Vec<EvaluationEvent> {
        self.state.lock().expect("repo lock").events.clone()
    }

    /// Every recorded evaluation, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<EvaluationRecord> {
        let state = self.state.lock().expect("repo lock");
        state
            .order
            .iter()
            .filter_map(|id| state.evaluations.get(&id.as_uuid()).map(|(r, _)| r.clone()))
            .collect()
    }
}

#[async_trait]
impl EvaluationRepo for InMemoryEvaluationRepo {
    async fn record(&self, record: &EvaluationRecord) -> Result<()> {
        let mut aggregate = Evaluation::default();
        let produced = aggregate.handle(&EvaluationCommand::Record(record.command()))?;
        for event in &produced {
            aggregate.apply(event);
        }
        let mut state = self.state.lock().expect("repo lock");
        state.events.extend(produced);
        state
            .evaluations
            .insert(record.id.as_uuid(), (record.clone(), aggregate));
        state.order.push(record.id);
        Ok(())
    }

    async fn evaluation(&self, id: EvaluationId) -> Result<Option<EvaluationRecord>> {
        Ok(self
            .state
            .lock()
            .expect("repo lock")
            .evaluations
            .get(&id.as_uuid())
            .map(|(record, _)| record.clone()))
    }

    async fn evaluations_of(&self, subject: EvaluationSubject) -> Result<Vec<EvaluationRecord>> {
        let state = self.state.lock().expect("repo lock");
        let mut out: Vec<EvaluationRecord> = state
            .evaluations
            .values()
            .filter(|(record, _)| record.subject == subject)
            .map(|(record, _)| record.clone())
            .collect();
        out.sort_by_key(|record| std::cmp::Reverse(record.created_at));
        Ok(out)
    }

    async fn proposal(&self, id: ProposalId) -> Result<Option<ProposalRow>> {
        Ok(self
            .state
            .lock()
            .expect("repo lock")
            .evaluations
            .values()
            .flat_map(|(record, _)| record.proposal_rows())
            .find(|row| row.id == id))
    }

    async fn proposals(
        &self,
        status: Option<ProposalStatus>,
        limit: usize,
    ) -> Result<Vec<ProposalRow>> {
        let state = self.state.lock().expect("repo lock");
        let mut rows: Vec<ProposalRow> = state
            .evaluations
            .values()
            .flat_map(|(record, _)| record.proposal_rows())
            .filter(|row| status.is_none_or(|s| row.status == s))
            .collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        rows.truncate(limit);
        Ok(rows)
    }

    async fn decide(
        &self,
        proposal_id: ProposalId,
        decision: Decision,
        by: &str,
    ) -> Result<ProposalRow> {
        let mut state = self.state.lock().expect("repo lock");
        let (record, aggregate) = state
            .evaluations
            .values_mut()
            .find(|(record, _)| record.proposals.iter().any(|p| p.id == proposal_id))
            .ok_or(EvaluatorError::ProposalNotFound(proposal_id))?;
        let produced = aggregate.handle(&decision.command(proposal_id, by))?;
        for event in &produced {
            aggregate.apply(event);
        }
        let decided_at = Utc::now();
        for proposal in &mut record.proposals {
            if proposal.id == proposal_id {
                proposal.status = decision.status();
            }
        }
        let mut row = record
            .proposal_rows()
            .into_iter()
            .find(|row| row.id == proposal_id)
            .ok_or(EvaluatorError::ProposalNotFound(proposal_id))?;
        state.events.extend(produced);
        row.decided_by = Some(by.to_owned());
        row.decided_at = Some(decided_at);
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevin_domain::TaskId;

    fn record(proposals: Vec<Proposal>) -> EvaluationRecord {
        EvaluationRecord {
            id: EvaluationId::new(),
            subject: EvaluationSubject::Task(TaskId::new()),
            run_id: RunId::new(),
            attempt_id: Some(AttemptId::new()),
            rubric_id: "code".to_owned(),
            judge_route: Route::new(WorkerKind::Fake, ModelAlias::new("fake").unwrap()),
            scores: vec![RubricScore::new("correctness", 8, "ok").unwrap()],
            overall: 0.8,
            verdict: Verdict::Accept,
            lessons: vec!["always run the tests".to_owned()],
            proposals,
            usage: Usage::ZERO,
            created_at: Utc::now(),
        }
    }

    fn proposal(kind: ProposalKind) -> Proposal {
        Proposal {
            id: ProposalId::new(),
            kind,
            body: "body".to_owned(),
            rationale: "why".to_owned(),
            status: ProposalStatus::Proposed,
        }
    }

    #[tokio::test]
    async fn recording_then_deciding_moves_the_row_out_of_the_inbox() {
        let repo = InMemoryEvaluationRepo::new();
        let p = proposal(ProposalKind::Routing);
        let rec = record(vec![p.clone()]);
        repo.record(&rec).await.unwrap();
        assert_eq!(
            repo.proposals(Some(ProposalStatus::Proposed), 10)
                .await
                .unwrap()
                .len(),
            1
        );

        let row = repo.decide(p.id, Decision::Accept, "vale").await.unwrap();
        assert_eq!(row.status, ProposalStatus::Accepted);
        assert_eq!(row.decided_by.as_deref(), Some("vale"));
        assert!(
            repo.proposals(Some(ProposalStatus::Proposed), 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(repo.events().len(), 2);
        // A second decision is refused by the aggregate.
        assert!(repo.decide(p.id, Decision::Reject, "vale").await.is_err());
    }
}
