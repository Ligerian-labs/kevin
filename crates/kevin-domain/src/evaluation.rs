//! The [`Evaluation`] aggregate (`plan/02-domain-model.md` §Aggregates › Evaluation).
//!
//! ```text
//! (none) ──RecordEvaluation──▶ recorded
//! recorded ──AcceptProposal{id}──▶ recorded (proposal accepted)
//! recorded ──RejectProposal{id}──▶ recorded (proposal rejected)
//! ```
//!
//! Proposals are never applied automatically; accepting/rejecting is a human
//! decision recorded here (`plan/06-memory-and-learning.md` §3.4).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::ids::{EvaluationId, ProposalId};
use crate::values::{
    EvaluationSubject, Proposal, ProposalKind, ProposalStatus, Route, RubricScore, Usage, Verdict,
};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const EVALUATION_AGGREGATE_TYPE: &str = "evaluation";

/// A proposal as drafted by the judge (status is always `proposed` on record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalDraft {
    /// Proposal id (assigned by the evaluator).
    pub id: ProposalId,
    /// Kind.
    pub kind: ProposalKind,
    /// The change.
    pub body: String,
    /// Why.
    #[serde(default)]
    pub rationale: String,
}

impl From<ProposalDraft> for Proposal {
    fn from(d: ProposalDraft) -> Self {
        Proposal {
            id: d.id,
            kind: d.kind,
            body: d.body,
            rationale: d.rationale,
            status: ProposalStatus::Proposed,
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Records a judge's evaluation (`evaluation.recorded`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordEvaluation {
    /// New evaluation id.
    pub evaluation_id: EvaluationId,
    /// What was judged.
    pub subject: EvaluationSubject,
    /// Rubric used.
    pub rubric_id: String,
    /// Judge route.
    pub judge_route: Route,
    /// Per-criterion scores.
    pub scores: Vec<RubricScore>,
    /// Overall 0..=1 (recomputed server-side from weights).
    pub overall: f32,
    /// Verdict.
    pub verdict: Verdict,
    /// Lessons learned.
    #[serde(default)]
    pub lessons: Vec<String>,
    /// Proposals raised.
    #[serde(default)]
    pub proposals: Vec<ProposalDraft>,
    /// Judge call usage.
    pub usage: Usage,
}

/// A human accepts a proposal (`evaluation.proposal_accepted`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptProposal {
    /// The proposal.
    pub proposal_id: ProposalId,
    /// Who.
    pub by: String,
    /// Why, in the operator's words. Recorded on the event so the decision is
    /// auditable months later (`plan/07` §API: `{note?}` on both verbs).
    #[serde(default)]
    pub note: Option<String>,
}

/// A human rejects a proposal (`evaluation.proposal_rejected`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectProposal {
    /// The proposal.
    pub proposal_id: ProposalId,
    /// Who.
    pub by: String,
    /// Why the proposal was turned down (`kevin proposals reject --note`).
    #[serde(default)]
    pub note: Option<String>,
}

/// Every command the [`Evaluation`] aggregate handles.
#[derive(Debug, Clone, PartialEq)]
pub enum EvaluationCommand {
    /// [`RecordEvaluation`].
    Record(RecordEvaluation),
    /// [`AcceptProposal`].
    AcceptProposal(AcceptProposal),
    /// [`RejectProposal`].
    RejectProposal(RejectProposal),
}

impl EvaluationCommand {
    /// `snake_case` command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            EvaluationCommand::Record(_) => "record_evaluation",
            EvaluationCommand::AcceptProposal(_) => "accept_proposal",
            EvaluationCommand::RejectProposal(_) => "reject_proposal",
        }
    }
}

impl From<RecordEvaluation> for EvaluationCommand {
    fn from(cmd: RecordEvaluation) -> Self {
        EvaluationCommand::Record(cmd)
    }
}

impl From<AcceptProposal> for EvaluationCommand {
    fn from(cmd: AcceptProposal) -> Self {
        EvaluationCommand::AcceptProposal(cmd)
    }
}

impl From<RejectProposal> for EvaluationCommand {
    fn from(cmd: RejectProposal) -> Self {
        EvaluationCommand::RejectProposal(cmd)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `evaluation` stream (internally tagged on `type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum EvaluationEvent {
    /// `evaluation.recorded`
    #[serde(rename = "evaluation.recorded")]
    Recorded {
        /// Evaluation id.
        evaluation_id: EvaluationId,
        /// Subject.
        subject: EvaluationSubject,
        /// Rubric.
        rubric_id: String,
        /// Judge route.
        judge_route: Route,
        /// Scores.
        scores: Vec<RubricScore>,
        /// Overall.
        overall: f32,
        /// Verdict.
        verdict: Verdict,
        /// Lessons.
        lessons: Vec<String>,
        /// Proposals (status `proposed`).
        proposals: Vec<Proposal>,
        /// Usage.
        usage: Usage,
    },
    /// `evaluation.proposal_accepted`
    #[serde(rename = "evaluation.proposal_accepted")]
    ProposalAccepted {
        /// Proposal.
        proposal_id: ProposalId,
        /// Who.
        by: String,
        /// The operator's note (schema v2; `null` in v1 payloads).
        #[serde(default)]
        note: Option<String>,
    },
    /// `evaluation.proposal_rejected`
    #[serde(rename = "evaluation.proposal_rejected")]
    ProposalRejected {
        /// Proposal.
        proposal_id: ProposalId,
        /// Who.
        by: String,
        /// The operator's note (schema v2; `null` in v1 payloads).
        #[serde(default)]
        note: Option<String>,
    },
}

impl EvaluationEvent {
    /// Every event type of the `evaluation` stream, in catalog order.
    pub const TYPES: [&'static str; 3] = [
        "evaluation.recorded",
        "evaluation.proposal_accepted",
        "evaluation.proposal_rejected",
    ];
}

impl EventMeta for EvaluationEvent {
    fn event_type(&self) -> &'static str {
        match self {
            EvaluationEvent::Recorded { .. } => "evaluation.recorded",
            EvaluationEvent::ProposalAccepted { .. } => "evaluation.proposal_accepted",
            EvaluationEvent::ProposalRejected { .. } => "evaluation.proposal_rejected",
        }
    }

    fn schema_version(&self) -> u16 {
        match self {
            EvaluationEvent::Recorded { .. } => 1,
            // v2 added `note`; `kevin_store::Upcasters::domain()` lifts stored
            // v1 payloads by inserting `note: null`.
            EvaluationEvent::ProposalAccepted { .. } | EvaluationEvent::ProposalRejected { .. } => {
                2
            }
        }
    }

    fn aggregate_type(&self) -> &'static str {
        EVALUATION_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The evaluation aggregate.
#[derive(Debug, Clone)]
pub struct Evaluation {
    version: u64,
    id: EvaluationId,
    subject: Option<EvaluationSubject>,
    rubric_id: String,
    judge_route: Option<Route>,
    scores: Vec<RubricScore>,
    overall: f32,
    verdict: Option<Verdict>,
    lessons: Vec<String>,
    proposals: Vec<Proposal>,
    usage: Usage,
}

impl Default for Evaluation {
    fn default() -> Self {
        Self {
            version: 0,
            id: EvaluationId::nil(),
            subject: None,
            rubric_id: String::new(),
            judge_route: None,
            scores: Vec::new(),
            overall: 0.0,
            verdict: None,
            lessons: Vec::new(),
            proposals: Vec::new(),
            usage: Usage::ZERO,
        }
    }
}

impl Evaluation {
    /// Typed id.
    #[must_use]
    pub const fn evaluation_id(&self) -> EvaluationId {
        self.id
    }

    /// Subject (after `evaluation.recorded`).
    #[must_use]
    pub const fn subject(&self) -> Option<EvaluationSubject> {
        self.subject
    }

    /// Rubric id.
    #[must_use]
    pub fn rubric_id(&self) -> &str {
        &self.rubric_id
    }

    /// Judge route.
    #[must_use]
    pub const fn judge_route(&self) -> Option<&Route> {
        self.judge_route.as_ref()
    }

    /// Scores.
    #[must_use]
    pub fn scores(&self) -> &[RubricScore] {
        &self.scores
    }

    /// Overall 0..=1.
    #[must_use]
    pub const fn overall(&self) -> f32 {
        self.overall
    }

    /// Verdict.
    #[must_use]
    pub const fn verdict(&self) -> Option<Verdict> {
        self.verdict
    }

    /// Lessons.
    #[must_use]
    pub fn lessons(&self) -> &[String] {
        &self.lessons
    }

    /// Proposals with their current status.
    #[must_use]
    pub fn proposals(&self) -> &[Proposal] {
        &self.proposals
    }

    /// Usage.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Finds a proposal.
    #[must_use]
    pub fn proposal(&self, id: ProposalId) -> Option<&Proposal> {
        self.proposals.iter().find(|p| p.id == id)
    }

    fn handle_record(&self, cmd: &RecordEvaluation) -> Result<Vec<EvaluationEvent>, DomainError> {
        if self.version > 0 {
            return Err(DomainError::AlreadyExists {
                aggregate: EVALUATION_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        if cmd.rubric_id.trim().is_empty() {
            return Err(DomainError::invalid_value("rubric_id", "must not be empty"));
        }
        for score in &cmd.scores {
            score.validate()?;
        }
        if !(0.0..=1.0).contains(&cmd.overall) {
            return Err(DomainError::invalid_value(
                "overall",
                "must be within 0..=1",
            ));
        }
        let mut ids: Vec<ProposalId> = cmd.proposals.iter().map(|p| p.id).collect();
        ids.sort();
        ids.dedup();
        if ids.len() != cmd.proposals.len() {
            return Err(DomainError::invalid_value(
                "proposals",
                "duplicate proposal id",
            ));
        }
        Ok(vec![EvaluationEvent::Recorded {
            evaluation_id: cmd.evaluation_id,
            subject: cmd.subject,
            rubric_id: cmd.rubric_id.clone(),
            judge_route: cmd.judge_route.clone(),
            scores: cmd.scores.clone(),
            overall: cmd.overall,
            verdict: cmd.verdict,
            lessons: cmd.lessons.clone(),
            proposals: cmd.proposals.iter().cloned().map(Proposal::from).collect(),
            usage: cmd.usage,
        }])
    }

    fn require_open_proposal(&self, id: ProposalId) -> Result<(), DomainError> {
        let proposal = self
            .proposal(id)
            .ok_or(DomainError::UnknownProposal { proposal_id: id })?;
        if proposal.status != ProposalStatus::Proposed {
            return Err(DomainError::ProposalAlreadyDecided {
                proposal_id: id,
                status: proposal.status,
            });
        }
        Ok(())
    }
}

impl Aggregate for Evaluation {
    type Command = EvaluationCommand;
    type Event = EvaluationEvent;

    const TYPE: &'static str = EVALUATION_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        self.id.as_uuid()
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn handle(&self, cmd: &EvaluationCommand) -> Result<Vec<EvaluationEvent>, DomainError> {
        match cmd {
            EvaluationCommand::Record(c) => self.handle_record(c),
            EvaluationCommand::AcceptProposal(c) => {
                self.require_recorded()?;
                self.require_open_proposal(c.proposal_id)?;
                Ok(vec![EvaluationEvent::ProposalAccepted {
                    proposal_id: c.proposal_id,
                    by: c.by.clone(),
                    note: c.note.clone(),
                }])
            }
            EvaluationCommand::RejectProposal(c) => {
                self.require_recorded()?;
                self.require_open_proposal(c.proposal_id)?;
                Ok(vec![EvaluationEvent::ProposalRejected {
                    proposal_id: c.proposal_id,
                    by: c.by.clone(),
                    note: c.note.clone(),
                }])
            }
        }
    }

    fn apply(&mut self, event: &EvaluationEvent) {
        self.version += 1;
        match event {
            EvaluationEvent::Recorded {
                evaluation_id,
                subject,
                rubric_id,
                judge_route,
                scores,
                overall,
                verdict,
                lessons,
                proposals,
                usage,
            } => {
                self.id = *evaluation_id;
                self.subject = Some(*subject);
                self.rubric_id.clone_from(rubric_id);
                self.judge_route = Some(judge_route.clone());
                self.scores.clone_from(scores);
                self.overall = *overall;
                self.verdict = Some(*verdict);
                self.lessons.clone_from(lessons);
                self.proposals.clone_from(proposals);
                self.usage = *usage;
            }
            EvaluationEvent::ProposalAccepted { proposal_id, .. } => {
                self.set_proposal_status(*proposal_id, ProposalStatus::Accepted);
            }
            EvaluationEvent::ProposalRejected { proposal_id, .. } => {
                self.set_proposal_status(*proposal_id, ProposalStatus::Rejected);
            }
        }
    }
}

impl Evaluation {
    fn require_recorded(&self) -> Result<(), DomainError> {
        if self.version == 0 {
            return Err(DomainError::NotFound {
                aggregate: EVALUATION_AGGREGATE_TYPE,
                id: self.id.as_uuid(),
            });
        }
        Ok(())
    }

    fn set_proposal_status(&mut self, id: ProposalId, status: ProposalStatus) {
        if let Some(p) = self.proposals.iter_mut().find(|p| p.id == id) {
            p.status = status;
        }
    }
}
