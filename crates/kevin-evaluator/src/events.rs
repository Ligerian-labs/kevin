//! `evaluation.*` events (`plan/02-domain-model.md` §Event catalog).
//!
//! The payloads are `kevin_domain::EvaluationEvent` (WS-01); this module only
//! turns one of them into the [`NewEvent`] the store appends, so the evaluation
//! context never re-declares the catalog.

use chrono::Utc;
use kevin_domain::aggregate::EventMeta as _;
use kevin_domain::{Actor, EvaluationEvent, EvaluationId, EventId, RunId};
use kevin_store::event_store::{NewEvent, StreamId};

pub use kevin_domain::evaluation::EVALUATION_AGGREGATE_TYPE as AGGREGATE_TYPE;

/// `evaluation.recorded`.
pub const RECORDED: &str = "evaluation.recorded";
/// `evaluation.proposal_accepted`.
pub const PROPOSAL_ACCEPTED: &str = "evaluation.proposal_accepted";
/// `evaluation.proposal_rejected`.
pub const PROPOSAL_REJECTED: &str = "evaluation.proposal_rejected";

/// The `Actor` every evaluator-emitted event carries.
#[must_use]
pub fn actor() -> Actor {
    Actor::system("evaluator")
}

/// The event stream of one evaluation.
#[must_use]
pub fn stream(id: EvaluationId) -> StreamId {
    StreamId::new(AGGREGATE_TYPE, id.as_uuid())
}

/// Wraps a domain event for the store, correlated on the run.
pub fn new_event(
    run_id: RunId,
    event: &EvaluationEvent,
    actor: Actor,
) -> Result<NewEvent, serde_json::Error> {
    Ok(NewEvent {
        event_id: EventId::new(),
        event_type: event.event_type(),
        schema_version: event.schema_version(),
        occurred_at: Utc::now(),
        correlation_id: run_id.as_uuid(),
        causation_id: None,
        actor,
        payload: serde_json::to_value(event)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_types_match_the_catalog() {
        assert_eq!(
            EvaluationEvent::TYPES,
            [RECORDED, PROPOSAL_ACCEPTED, PROPOSAL_REJECTED]
        );
        assert_eq!(AGGREGATE_TYPE, "evaluation");
    }

    #[test]
    fn a_wrapped_event_carries_the_catalog_metadata() {
        let run_id = RunId::new();
        let event = EvaluationEvent::ProposalRejected {
            proposal_id: kevin_domain::ProposalId::new(),
            by: "vale".to_owned(),
        };
        let wrapped = new_event(run_id, &event, actor()).unwrap();
        assert_eq!(wrapped.event_type, PROPOSAL_REJECTED);
        assert_eq!(wrapped.schema_version, 1);
        assert_eq!(wrapped.correlation_id, run_id.as_uuid());
        assert_eq!(wrapped.payload["type"], PROPOSAL_REJECTED);
    }
}
