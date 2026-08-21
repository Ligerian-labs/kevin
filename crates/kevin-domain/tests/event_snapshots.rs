//! Event payloads: one JSON snapshot per event type, serde round-trip (typed
//! and through `DomainEvent`), `event_type`/`schema_version` consistency,
//! and the envelope wrapping (`plan/02-domain-model.md` §Event catalog).

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;

use chrono::TimeZone;
use kevin_domain::aggregate::EventMeta;
use kevin_domain::envelope::{Actor, EventEnvelope};
use kevin_domain::event::DomainEvent;
use kevin_domain::ids::EventId;
use kevin_domain::kinds::FailureClass;
use kevin_domain::values::Usage;
use kevin_testkit::given_when_then::{
    evaluation, ids, memory_item, question, route_score, run, task, values,
};

/// One representative instance of every event type in the catalog.
fn catalog_instances() -> Vec<DomainEvent> {
    let u = values::usage();
    let q = ids::question_id(1);
    let mut events: Vec<DomainEvent> = vec![
        run::started().into(),
        run::understanding_started().into(),
        run::understanding_completed_with_questions(vec![q]).into(),
        run::question_answered(q, 0).into(),
        run::plan_proposed().into(),
        run::plan_approved().into(),
        run::plan_rejected().into(),
        run::execution_started().into(),
        run::usage_recorded().into(),
        run::task_terminal_noted(ids::task_id(1), false, u + u + u).into(),
        run::budget_exhausted().into(),
        run::integrated().into(),
        run::evaluated().into(),
        run::completed(u + u + u + u).into(),
        run::failed(u).into(),
        run::cancelled().into(),
        run::evaluation_requested().into(),
        task::created().into(),
        task::routed().into(),
        task::attempt_started(1).into(),
        task::progressed(1).into(),
        task::input_requested(1, q).into(),
        task::input_provided(1, q).into(),
        task::attempt_succeeded(1).into(),
        task::attempt_failed(1, FailureClass::Transient, true).into(),
        task::retried(2).into(),
        task::cancelled().into(),
        task::skipped().into(),
        question::asked_with_default().into(),
        question::answered().into(),
        question::expired(true).into(),
        evaluation::recorded().into(),
        evaluation::proposal_accepted().into(),
        evaluation::proposal_rejected().into(),
        route_score::score_updated_after_success().into(),
        memory_item::stored().into(),
        memory_item::superseded().into(),
        memory_item::forgotten().into(),
    ];
    events.sort_by_key(|e| {
        DomainEvent::catalog()
            .iter()
            .position(|t| *t == e.event_type())
    });
    events
}

#[test]
fn ac_ws01_3_every_event_round_trips_with_schema_version_and_has_a_snapshot() {
    let instances = catalog_instances();
    let catalog: BTreeSet<&str> = DomainEvent::catalog().into_iter().collect();
    let covered: BTreeSet<&str> = instances.iter().map(EventMeta::event_type).collect();
    assert_eq!(
        covered, catalog,
        "every catalog event type needs an instance"
    );
    assert_eq!(catalog.len(), 38);

    for event in &instances {
        let event_type = event.event_type();
        assert_eq!(event.schema_version(), 1);
        let value = serde_json::to_value(event).unwrap();
        // Payload is self-describing: the `type` key equals the catalog name.
        assert_eq!(
            value["type"],
            serde_json::Value::String(event_type.to_owned())
        );
        // Catalog naming: `<context>.<past_tense>`.
        let (context, _) = event_type.split_once('.').unwrap();
        assert!(
            ["run", "task", "question", "evaluation", "routing", "memory"].contains(&context),
            "{event_type} has an unknown context"
        );
        // Round trip through the untagged union and the typed enum.
        let back: DomainEvent = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            &back, event,
            "{event_type} did not round-trip through DomainEvent"
        );
        match event {
            DomainEvent::Run(e) => {
                let typed: kevin_domain::run::RunEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
            DomainEvent::Task(e) => {
                let typed: kevin_domain::task::TaskEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
            DomainEvent::Question(e) => {
                let typed: kevin_domain::question::QuestionEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
            DomainEvent::Evaluation(e) => {
                let typed: kevin_domain::evaluation::EvaluationEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
            DomainEvent::RouteScore(e) => {
                let typed: kevin_domain::route_score::RouteScoreEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
            DomainEvent::MemoryItem(e) => {
                let typed: kevin_domain::memory_item::MemoryItemEvent =
                    serde_json::from_value(value.clone()).unwrap();
                assert_eq!(&typed, e);
            }
        }
        // Snapshot named `<event_type>.v<schema_version>`.
        let name = format!("{event_type}.v{}", event.schema_version());
        insta::assert_json_snapshot!(name, value);
    }
}

#[test]
fn envelope_round_trips_with_domain_event_payload() {
    let run_id = ids::run_id();
    let envelope = EventEnvelope {
        event_id: EventId::from_uuid(ids::fixture_uuid(0xee)),
        event_type: "run.started",
        schema_version: 1,
        occurred_at: chrono::Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap(),
        aggregate_type: "run",
        aggregate_id: run_id.as_uuid(),
        aggregate_version: 1,
        correlation_id: run_id.as_uuid(),
        causation_id: None,
        actor: Actor::user("valentin"),
        payload: DomainEvent::from(run::started()),
    };
    assert_eq!(envelope.event_type, envelope.payload.event_type());
    assert_eq!(envelope.aggregate_type, envelope.payload.aggregate_type());
    assert_eq!(envelope.schema_version, envelope.payload.schema_version());
    let json = serde_json::to_string(&envelope).unwrap();
    let back: EventEnvelope<DomainEvent> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, envelope);
    // Erased payload → typed payload by event_type.
    let erased: EventEnvelope<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert_eq!(erased.payload["type"], "run.started");
    let typed = erased
        .try_map_payload(serde_json::from_value::<kevin_domain::run::RunEvent>)
        .unwrap();
    assert_eq!(typed.payload, run::started());
}

#[test]
fn domain_event_accessors_and_catalog_order() {
    let e = DomainEvent::from(task::skipped());
    assert!(e.as_task().is_some());
    assert!(e.as_run().is_none());
    assert!(e.as_question().is_none());
    assert!(e.as_evaluation().is_none());
    assert!(e.as_route_score().is_none());
    assert!(e.as_memory_item().is_none());
    let catalog = DomainEvent::catalog();
    assert_eq!(catalog[0], "run.started");
    assert_eq!(*catalog.last().unwrap(), "memory.item_forgotten");
    assert_eq!(
        catalog.iter().collect::<BTreeSet<_>>().len(),
        catalog.len(),
        "no duplicates"
    );
    assert_eq!(Usage::ZERO.total_tokens(), 0);
}
