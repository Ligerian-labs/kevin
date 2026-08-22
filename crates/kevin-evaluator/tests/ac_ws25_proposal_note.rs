//! WS-25 follow-up — `kevin proposals accept|reject --note` persists the note.
//!
//! The flag used to print the note back at the operator and drop it, because
//! `evaluation.proposal_rejected` had no field for it.
//! `plan/07-api-and-tui.md` specifies the note on both the CLI verb and the
//! API body, so the honest fix was to record it: schema **v2** of both
//! decision events carries `note?`, and `Upcasters::domain()` lifts stored v1
//! payloads to `note: null`.
//!
//! Needs Postgres; skips cleanly without it.

mod common;

use std::sync::Arc;

use chrono::Utc;
use common::{
    GOLDEN_ACCEPT, config, executor_route, implement, implement_evidence, registry, task_ids,
};
use kevin_domain::{Actor, EvaluationId, EventId, ProposalId, ProposalStatus, RunId};
use kevin_evaluator::repo::Decision;
use kevin_evaluator::{
    AutoApply, EvaluationRepo, EvaluationRequest, Evaluator, PgEvaluationRepo, Proposals,
};
use kevin_store::{EventStore, NewEvent, PgEventStore, StreamId, Upcasters};
use kevin_testkit::pg::TestDb;
use kevin_testkit::skip_unless_pg;
use serde_json::{Value, json};

/// A database with one recorded evaluation that raised proposals.
struct Seeded {
    db: TestDb,
    repo: Arc<PgEvaluationRepo>,
    /// A proposal in `proposed` state.
    proposal: ProposalId,
    /// Keeps the fake worker's transcript directory alive.
    _dir: tempfile::TempDir,
}

impl Seeded {
    async fn new() -> Self {
        let db = TestDb::new().await;
        let events = Arc::new(PgEventStore::new(db.pool().clone()));
        let repo = Arc::new(PgEvaluationRepo::new(db.pool().clone(), events));
        let (dir, workers) = registry(GOLDEN_ACCEPT, true);
        let evaluator = Evaluator::new(
            config(true),
            Arc::new(workers),
            kevin_worker::Workspace::in_place(dir.path()),
            Arc::clone(&repo) as Arc<dyn EvaluationRepo>,
            AutoApply::new([]),
        );
        let (run_id, task_id) = task_ids();
        let id = evaluator
            .evaluate(
                EvaluationRequest::for_task(run_id, task_id, implement(), implement_evidence())
                    .with_executor_route(executor_route()),
            )
            .await
            .expect("judged");
        let record = repo.evaluation(id).await.unwrap().expect("row");
        let proposal = record
            .proposals
            .first()
            .expect("the golden judge raises proposals")
            .id;
        Self {
            db,
            repo,
            proposal,
            _dir: dir,
        }
    }

    /// Payloads of every `event_type` event, in order.
    async fn events_of_type(&self, event_type: &str) -> Vec<Value> {
        sqlx::query_scalar::<_, Value>(
            "SELECT payload FROM core.events WHERE event_type = $1 ORDER BY position",
        )
        .bind(event_type)
        .fetch_all(self.db.pool())
        .await
        .expect("read core.events")
    }

    async fn close(self) {
        self.db.close().await;
    }
}

/// The note reaches `core.events` and comes back on the event.
#[tokio::test]
async fn ac_ws25_11_1_a_rejection_note_is_persisted_on_the_event() {
    skip_unless_pg!();
    let fx = Seeded::new().await;

    let row = fx
        .repo
        .decide(
            fx.proposal,
            Decision::Reject,
            "valentin",
            Some("we already tried this alias last week".to_owned()),
        )
        .await
        .expect("reject");
    assert_eq!(row.status, ProposalStatus::Rejected);
    assert_eq!(row.decided_by.as_deref(), Some("valentin"));

    let events = fx.events_of_type("evaluation.proposal_rejected").await;
    assert_eq!(events.len(), 1, "one rejection event");
    assert_eq!(
        events[0]["note"], "we already tried this alias last week",
        "the note must survive into the event payload"
    );
    assert_eq!(events[0]["by"], "valentin");
    fx.close().await;
}

/// A decision without a note is still valid and records `null` — the field is
/// optional, not required.
#[tokio::test]
async fn ac_ws25_11_2_a_decision_without_a_note_records_null() {
    skip_unless_pg!();
    let fx = Seeded::new().await;

    fx.repo
        .decide(fx.proposal, Decision::Accept, "valentin", None)
        .await
        .expect("accept");

    let events = fx.events_of_type("evaluation.proposal_accepted").await;
    assert_eq!(events.len(), 1);
    assert!(
        events[0]["note"].is_null(),
        "an absent note is null, not missing: {}",
        events[0]
    );
    fx.close().await;
}

/// The inbox verb passes the note through, not just the repository.
#[tokio::test]
async fn ac_ws25_11_4_the_inbox_verbs_record_the_operator_note() {
    skip_unless_pg!();
    let fx = Seeded::new().await;
    let inbox = Proposals::new(Arc::clone(&fx.repo) as Arc<dyn EvaluationRepo>);

    inbox
        .reject(fx.proposal, "valentin", Some("not now".to_owned()))
        .await
        .expect("reject");

    let events = fx.events_of_type("evaluation.proposal_rejected").await;
    assert_eq!(events[0]["note"], "not now");
    fx.close().await;
}

/// A v1 payload written before the field existed still reads back — as v2,
/// with `note: null`. Without the upcaster, a historical decision event would
/// read as a different shape than a fresh one.
#[tokio::test]
async fn ac_ws25_11_3_a_stored_v1_decision_upcasts_to_v2_with_a_null_note() {
    skip_unless_pg!();
    let db = TestDb::new().await;
    // `PgEventStore::new` installs `Upcasters::domain()`.
    let store = PgEventStore::new(db.pool().clone());
    let evaluation = EvaluationId::new();
    let stream = StreamId::new("evaluation", evaluation.as_uuid());

    // Exactly what v1 wrote: no `note` key at all.
    let v1 = json!({
        "type": "evaluation.proposal_rejected",
        "proposal_id": ProposalId::new(),
        "by": "valentin",
    });
    store
        .append(
            &stream,
            0,
            &[NewEvent {
                event_id: EventId::new(),
                event_type: "evaluation.proposal_rejected",
                schema_version: 1,
                occurred_at: Utc::now(),
                correlation_id: RunId::new().as_uuid(),
                causation_id: None,
                actor: Actor::user("valentin"),
                payload: v1,
            }],
        )
        .await
        .expect("append a v1 event");

    let read = store.load_stream(&stream, 0).await.expect("load_stream");
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].envelope.schema_version, 2, "upcast to the latest");
    assert!(
        read[0].envelope.payload.get("note").is_some(),
        "the upcaster must add the key: {}",
        read[0].envelope.payload
    );
    assert!(read[0].envelope.payload["note"].is_null());

    // And it deserialises as the current typed event.
    let typed: kevin_domain::EvaluationEvent =
        serde_json::from_value(read[0].envelope.payload.clone()).expect("typed");
    assert!(matches!(
        typed,
        kevin_domain::EvaluationEvent::ProposalRejected { note: None, .. }
    ));

    // A store without the registry leaves the payload exactly as written —
    // which is what makes the registry, not serde's `default`, the thing under
    // test here.
    let raw = PgEventStore::with_upcasters(db.pool().clone(), Upcasters::new());
    let unlifted = raw.load_stream(&stream, 0).await.expect("load_stream");
    assert_eq!(unlifted[0].envelope.schema_version, 1);
    assert!(unlifted[0].envelope.payload.get("note").is_none());

    db.close().await;
}
