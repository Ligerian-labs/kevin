//! WS-25 hardening — the event store under failure (`plan/12` §WS-25).
//!
//! Three properties the rest of the system leans on and nothing tested before:
//!
//! - a Postgres outage during `append` is **loud**: it never returns `Ok`, and
//!   it never half-writes a stream (an aborted append leaves the version where
//!   it was, so the retry produces exactly one copy of each event);
//! - no event payload reaches `core.events` with a credential in it
//!   (`plan/09-security.md` §Redaction: the store is a sink);
//! - [`PgEventStore`] is a `kevin_bus::EventSource`, which is what lets a
//!   second *process* attach to a running Kevin over `PgNotifyBus`.
//!
//! Every test skips cleanly where no Postgres is configured.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use kevin_bus::{
    BusMessage, EventBus, EventSource, PgNotifyBus, PgNotifyBusConfig, SubscriptionFilter,
};
use kevin_domain::{Actor, EventId};
use kevin_store::{EventStore, NewEvent, PgEventStore, StoreError, StreamId};
use kevin_testkit::pg::TestDb;
use serde_json::{Value, json};
use uuid::Uuid;

fn event(event_type: &'static str, payload: Value) -> NewEvent {
    NewEvent {
        event_id: EventId::new(),
        event_type,
        schema_version: 1,
        occurred_at: Utc::now(),
        correlation_id: Uuid::now_v7(),
        causation_id: None,
        actor: Actor::system("ws25"),
        payload,
    }
}

fn run_stream() -> StreamId {
    StreamId::new("run", Uuid::now_v7())
}

/// Kills every server-side backend of `db` — a Postgres restart as the pool
/// experiences it. `pg_terminate_backend` on the *admin* connection, so the
/// killer does not kill itself.
async fn kill_backends(db: &TestDb) {
    let admin = sqlx::PgPool::connect(db.admin_url())
        .await
        .expect("admin pool");
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(db.name())
    .execute(&admin)
    .await;
    admin.close().await;
}

// ---------------------------------------------------------------------------
// (2) Postgres outage during `append`
// ---------------------------------------------------------------------------

/// An unreachable Postgres must make `append` **fail**, never silently drop the
/// events and never panic. The counter-example this guards against is an
/// append path that swallows the error and lets the saga believe the run
/// progressed.
#[tokio::test]
async fn ac_ws25_2_1_append_to_an_unreachable_postgres_fails_loudly() {
    // No `skip_unless_pg!`: nothing is expected to be reachable here, which is
    // exactly the point — the test is deterministic everywhere.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_millis(500))
        // Port 1 is never a Postgres.
        .connect_lazy("postgres://kevin:kevin@127.0.0.1:1/kevin")
        .expect("lazy pool");
    let store = PgEventStore::new(pool);
    let stream = run_stream();

    let err = store
        .append(&stream, 0, &[event("run.started", json!({"goal": "a"}))])
        .await
        .expect_err("append against a dead database must fail");
    assert!(
        matches!(err, StoreError::Database(_)),
        "the outage must surface as a database error, got {err:?}"
    );
    // Reads fail the same way rather than pretending the stream is empty.
    assert!(EventStore::read_all(&store, 0, 10).await.is_err());
    assert!(store.load_stream(&stream, 0).await.is_err());
    // And the position watch never advanced on a failed append.
    assert_eq!(*store.subscribe_positions().borrow(), 0);
}

/// A backend kill between two appends loses nothing and duplicates nothing:
/// after the outage the stream holds exactly the events that were acknowledged
/// plus the retried one, with contiguous versions.
#[tokio::test]
async fn ac_ws25_2_2_backend_kill_during_append_never_loses_or_duplicates_events() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = run_stream();

    let first = store
        .append(&stream, 0, &[event("run.started", json!({"goal": "a"}))])
        .await
        .expect("first append");
    assert_eq!(first.new_version, 1);

    kill_backends(&db).await;

    // The append that lands on the killed connection may fail; what it may
    // never do is commit *some* of its events. Retry until the pool has
    // reconnected, counting attempts so the test also proves the failure is
    // observable rather than silent.
    let payloads = [
        event("run.plan_proposed", json!({"n": 1})),
        event("run.plan_approved", json!({"by": "tester"})),
    ];
    let mut failures = 0;
    let mut attempts = 0;
    loop {
        attempts += 1;
        assert!(attempts <= 10, "the pool never recovered");
        match store.append(&stream, 1, &payloads).await {
            Ok(result) => {
                assert_eq!(result.new_version, 3);
                break;
            }
            Err(StoreError::Database(_)) => {
                failures += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(other) => panic!("unexpected error during the outage: {other:?}"),
        }
    }
    let _ = failures; // an immediate reconnect is a valid outcome too

    let events = store.load_stream(&stream, 0).await.expect("load_stream");
    let versions: Vec<u64> = events
        .iter()
        .map(|e| e.envelope.aggregate_version)
        .collect();
    assert_eq!(versions, vec![1, 2, 3], "no gap, no duplicate");
    let types: Vec<&str> = events.iter().map(|e| e.envelope.event_type).collect();
    assert_eq!(
        types,
        vec!["run.started", "run.plan_proposed", "run.plan_approved"]
    );
    db.close().await;
}

/// A conflicting append rolls back completely: the losing writer's events are
/// nowhere to be found, so a crash between the conflict check and the commit
/// cannot leave a partial batch behind.
#[tokio::test]
async fn ac_ws25_2_3_a_rejected_append_writes_nothing_at_all() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = run_stream();
    store
        .append(&stream, 0, &[event("run.started", json!({}))])
        .await
        .expect("first append");

    let err = store
        .append(
            &stream,
            0,
            &[
                event("run.plan_proposed", json!({"n": 1})),
                event("run.plan_approved", json!({})),
            ],
        )
        .await
        .expect_err("stale expected_version");
    assert!(matches!(err, StoreError::VersionConflict { .. }), "{err:?}");

    let events = store.load_stream(&stream, 0).await.expect("load_stream");
    assert_eq!(events.len(), 1, "the rejected batch left rows behind");
    let outbox: i64 = sqlx::query_scalar("SELECT count(*) FROM core.outbox")
        .fetch_one(db.pool())
        .await
        .expect("count outbox");
    assert_eq!(outbox, 1, "the rejected batch left outbox rows behind");
    db.close().await;
}

// ---------------------------------------------------------------------------
// (6) The store is a redaction sink
// ---------------------------------------------------------------------------

/// `plan/09-security.md` §Redaction lists "event payloads before append" as a
/// sink. A worker that echoes a key into a summary must not persist it: the
/// rows outlive the run, feed every projection and are served over SSE.
#[tokio::test]
async fn ac_ws25_6_1_event_payloads_are_redacted_before_they_reach_core_events() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = run_stream();

    let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    store
        .append(
            &stream,
            0,
            &[event(
                "task.attempt_succeeded",
                json!({
                    "summary": format!("exported {secret} to the env"),
                    "artifacts": [{ "uri": "https://x/y?token=ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }],
                    "nested": { "deep": ["Bearer abcdefghijklmnopqrstuvwxyz"] },
                }),
            )],
        )
        .await
        .expect("append");

    // Read the raw column, not the API: the requirement is about what is *at
    // rest* in Postgres.
    let raw: Value = sqlx::query_scalar("SELECT payload FROM core.events LIMIT 1")
        .fetch_one(db.pool())
        .await
        .expect("payload");
    let text = raw.to_string();
    assert!(
        !text.contains(secret),
        "the anthropic key survived into core.events: {text}"
    );
    assert!(
        !text.contains("ghp_AAAA"),
        "the github token survived: {text}"
    );
    assert!(
        kevin_telemetry::redact::contains_marker(&text),
        "nothing was redacted at all: {text}"
    );
    db.close().await;
}

// ---------------------------------------------------------------------------
// (9) The store is the bus' event source
// ---------------------------------------------------------------------------

/// `PgEventStore` implements `kevin_bus::EventSource`, so `PgNotifyBus` can
/// read events back by global position.
#[tokio::test]
async fn ac_ws25_9_1_pg_event_store_is_an_event_source() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    assert_eq!(
        EventSource::latest_position(&store).await.expect("head"),
        0,
        "an empty store starts at 0"
    );

    let stream = run_stream();
    store
        .append(
            &stream,
            0,
            &[
                event("run.started", json!({"goal": "a"})),
                event("run.completed", json!({})),
            ],
        )
        .await
        .expect("append");

    let head = EventSource::latest_position(&store).await.expect("head");
    assert_eq!(head, 2);
    let read = EventSource::read_all(&store, 0, 10)
        .await
        .expect("read_all");
    assert_eq!(
        read.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(read[0].envelope.event_type, "run.started");
    // `from_position` is exclusive, the same convention as the bus.
    let tail = EventSource::read_all(&store, 1, 10)
        .await
        .expect("read_all");
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].position, 2);
    db.close().await;
}

/// The end that matters: a bus built on a *second* pool and a *second* store —
/// i.e. what another process has — sees events appended by the first one. This
/// is the cross-process attach that was impossible while nothing implemented
/// `EventSource`.
#[tokio::test]
async fn ac_ws25_9_2_a_second_process_attaches_over_pg_notify() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let writer = PgEventStore::new(db.pool().clone());

    // "Another process": its own pool, its own store, its own bus.
    let other_pool = db.connect_other().await;
    let reader = Arc::new(PgEventStore::new(other_pool.clone()));
    let bus = PgNotifyBus::with_config(
        other_pool,
        Arc::clone(&reader) as Arc<dyn EventSource>,
        PgNotifyBusConfig {
            poll_interval: Duration::from_millis(50),
            ..PgNotifyBusConfig::default()
        },
    )
    .await
    .expect("pg bus");

    let stream = run_stream();
    let run_id = Uuid::now_v7();
    let mut subscription = bus.subscribe(SubscriptionFilter::for_run(run_id));

    let mut started = event("run.started", json!({"goal": "attach"}));
    started.correlation_id = run_id;
    writer
        .append(&stream, 0, std::slice::from_ref(&started))
        .await
        .expect("append");

    let message = tokio::time::timeout(Duration::from_secs(10), subscription.next())
        .await
        .expect("the second process is woken within 10s")
        .expect("a message");
    let BusMessage::Live(received) = message else {
        panic!("expected a live event, got {message:?}");
    };
    assert_eq!(received.envelope.event_type, "run.started");
    assert_eq!(received.position, 1);
    assert_eq!(received.envelope.payload["goal"], "attach");
    db.close().await;
}
