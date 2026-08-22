//! The proposals inbox (`plan/06-memory-and-learning.md` §3.4,
//! `plan/07-api-and-tui.md` `kevin proposals`).
//!
//! A judge never changes Kevin: it raises proposals. `accept` emits
//! `evaluation.proposal_accepted` and prints the exact change for the human to
//! apply — with one exception, spelled out in §3.4: a **routing** proposal may
//! be applied on accept, because a route outcome is bounded and self-correcting.
//! Prompt and config proposals are never written by Kevin.
//!
//! A routing proposal's `body` is a single JSON object (the judge prompt says
//! so):
//!
//! ```json
//! {"action": "boost", "task_kind": "implement", "alias": "gpt56-codex", "quality": 0.9}
//! ```
//!
//! `boost` and `penalize` become one `RecordRouteOutcome`; `reset` needs
//! `kevin routes reset` and is reported, not applied.

use std::sync::Arc;

use chrono::Utc;
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome};
use kevin_domain::{ModelAlias, ProposalId, ProposalKind, ProposalStatus, TaskKind};
use serde::{Deserialize, Serialize};

use crate::error::{EvaluatorError, Result};
use crate::repo::{Decision, EvaluationRepo, ProposalRow};
use crate::router_port::RouterPort;

/// Default number of proposals `kevin proposals ls` prints.
pub const DEFAULT_LIMIT: usize = 50;

/// What a routing proposal asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingAction {
    /// Record a successful outcome with the given quality.
    Boost,
    /// Record a failed outcome.
    Penalize,
    /// Reset the pair to its prior (`kevin routes reset`; not auto-applied).
    Reset,
}

/// A parsed routing proposal body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingDirective {
    /// What to do.
    pub action: RoutingAction,
    /// The task kind it applies to.
    pub task_kind: TaskKind,
    /// The alias it applies to.
    pub alias: ModelAlias,
    /// Quality in `0..=1`; defaults to `0.9` for a boost, `0.1` for a penalty.
    #[serde(default)]
    pub quality: Option<f32>,
}

impl RoutingDirective {
    /// Parses a routing proposal body; `None` when it is prose rather than the
    /// documented JSON object.
    #[must_use]
    pub fn parse(body: &str) -> Option<Self> {
        serde_json::from_str::<Self>(body.trim()).ok()
    }

    /// The `RecordRouteOutcome` this directive stands for, if it is applicable.
    #[must_use]
    pub fn as_outcome(&self) -> Option<RecordRouteOutcome> {
        let (success, quality) = match self.action {
            RoutingAction::Boost => (true, self.quality.unwrap_or(0.9)),
            RoutingAction::Penalize => (false, self.quality.unwrap_or(0.1)),
            RoutingAction::Reset => return None,
        };
        Some(RecordRouteOutcome {
            task_kind: self.task_kind.clone(),
            alias: self.alias.clone(),
            success,
            quality: Some(quality.clamp(0.0, 1.0)),
            cost_usd: None,
            wall_ms: 0,
            failure_class: None,
            recorded_at: Utc::now(),
            prior: BetaPrior::default(),
        })
    }
}

/// What accepting a proposal did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptOutcome {
    /// The updated row.
    pub proposal: ProposalRow,
    /// `true` when a routing directive was applied to the router.
    pub applied: bool,
    /// What a human must do, when Kevin did not do it.
    pub manual: Option<String>,
}

/// The inbox service behind `kevin proposals` and `/api/v1/proposals`.
#[derive(Debug, Clone)]
pub struct Proposals {
    repo: Arc<dyn EvaluationRepo>,
    router: Option<Arc<dyn RouterPort>>,
}

impl Proposals {
    /// An inbox over `repo`; without a router, routing proposals are reported
    /// for a human like every other kind.
    #[must_use]
    pub fn new(repo: Arc<dyn EvaluationRepo>) -> Self {
        Self { repo, router: None }
    }

    /// Wires the router so accepted routing proposals apply.
    #[must_use]
    pub fn with_router(mut self, router: Arc<dyn RouterPort>) -> Self {
        self.router = Some(router);
        self
    }

    /// Lists proposals with `status` (default: the whole inbox), newest first.
    pub async fn list(
        &self,
        status: Option<ProposalStatus>,
        limit: usize,
    ) -> Result<Vec<ProposalRow>> {
        self.repo.proposals(status, limit).await
    }

    /// One proposal.
    pub async fn get(&self, id: ProposalId) -> Result<ProposalRow> {
        self.repo
            .proposal(id)
            .await?
            .ok_or(EvaluatorError::ProposalNotFound(id))
    }

    /// Accepts a proposal: emits `evaluation.proposal_accepted` and applies it
    /// when it is a routing directive the router can take.
    pub async fn accept(&self, id: ProposalId, by: &str) -> Result<AcceptOutcome> {
        let row = self.repo.decide(id, Decision::Accept, by).await?;
        let (applied, manual) = self.apply(&row).await?;
        metrics::counter!(
            crate::evaluator::EVAL_PROPOSALS,
            "kind" => crate::repo::kind_str(row.kind),
            "status" => crate::repo::status_str(row.status),
        )
        .increment(1);
        tracing::info!(
            event = kevin_telemetry::events::eval::PROPOSAL_ACCEPTED,
            proposal = %row.id,
            kind = crate::repo::kind_str(row.kind),
            applied,
            "proposal accepted"
        );
        Ok(AcceptOutcome {
            proposal: row,
            applied,
            manual,
        })
    }

    /// Rejects a proposal (`evaluation.proposal_rejected`).
    pub async fn reject(&self, id: ProposalId, by: &str) -> Result<ProposalRow> {
        let row = self.repo.decide(id, Decision::Reject, by).await?;
        metrics::counter!(
            crate::evaluator::EVAL_PROPOSALS,
            "kind" => crate::repo::kind_str(row.kind),
            "status" => crate::repo::status_str(row.status),
        )
        .increment(1);
        tracing::info!(
            event = kevin_telemetry::events::eval::PROPOSAL_REJECTED,
            proposal = %row.id,
            kind = crate::repo::kind_str(row.kind),
            "proposal rejected"
        );
        Ok(row)
    }

    /// Applies an accepted routing proposal; returns `(applied, manual step)`.
    async fn apply(&self, row: &ProposalRow) -> Result<(bool, Option<String>)> {
        if row.kind != ProposalKind::Routing {
            return Ok((false, Some(manual_note(row))));
        }
        let Some(directive) = RoutingDirective::parse(&row.body) else {
            return Ok((
                false,
                Some(format!(
                    "routing proposal body is not the documented JSON object; apply it by hand:\n{}",
                    row.body
                )),
            ));
        };
        let Some(outcome) = directive.as_outcome() else {
            return Ok((
                false,
                Some(format!(
                    "run `kevin routes reset --kind {} --alias {}`",
                    directive.task_kind, directive.alias
                )),
            ));
        };
        let Some(router) = self.router.as_ref() else {
            return Ok((
                false,
                Some("no router is wired; apply the directive with `kevin routes`".to_owned()),
            ));
        };
        router
            .record_outcome(outcome, None)
            .await
            .map_err(|e| EvaluatorError::AutoApply {
                part: "routing",
                message: e.to_string(),
            })?;
        Ok((true, None))
    }
}

/// What a human has to do with a prompt/config proposal.
fn manual_note(row: &ProposalRow) -> String {
    match row.kind {
        ProposalKind::Prompt => {
            "prompt proposals are never written by Kevin — apply it to the prompt yourself"
                .to_owned()
        }
        ProposalKind::Config => {
            "config proposals are never written by Kevin — edit the TOML yourself".to_owned()
        }
        ProposalKind::Routing => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_routing_body_parses_into_an_outcome() {
        let directive = RoutingDirective::parse(
            "{\"action\":\"boost\",\"task_kind\":\"implement\",\"alias\":\"gpt56-codex\"}",
        )
        .expect("parses");
        assert_eq!(directive.action, RoutingAction::Boost);
        let outcome = directive.as_outcome().expect("outcome");
        assert!(outcome.success);
        assert_eq!(outcome.quality, Some(0.9));
        assert_eq!(outcome.task_kind, TaskKind::Implement);

        let penalty = RoutingDirective::parse(
            "{\"action\":\"penalize\",\"task_kind\":\"test\",\"alias\":\"fake\",\"quality\":0.2}",
        )
        .unwrap()
        .as_outcome()
        .unwrap();
        assert!(!penalty.success);
        assert_eq!(penalty.quality, Some(0.2));

        let reset = RoutingDirective::parse(
            "{\"action\":\"reset\",\"task_kind\":\"test\",\"alias\":\"fake\"}",
        )
        .unwrap();
        assert!(reset.as_outcome().is_none());
    }

    #[test]
    fn prose_is_not_a_directive() {
        assert!(RoutingDirective::parse("prefer codex for implement tasks").is_none());
        assert!(RoutingDirective::parse("{\"action\":\"delete\"}").is_none());
    }
}
