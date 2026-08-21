//! WS-03 acceptance criteria (`plan/12-workstreams.md` §WS-03) against a real
//! Postgres (`kevin_testkit::pg::TestDb`, one database per test).
//!
//! (5) also has a CLI half in `crates/kevin-cli/tests/ac_ws03_db_commands.rs`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use kevin_domain::{Actor, CommandId, EventId};
use kevin_store::migrate::{self, MigratePolicy, MigrationState};
use kevin_store::{
    Begun, Checkpoints, CommandLog, CompleteOutcome, DeliveryError, EventStore, NOTIFY_CHANNEL,
    NewEvent, Outbox, PgEventStore, Snapshots, StoreError, StreamId, Upcasters,
};
use kevin_testkit::pg::TestDb;
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn event(event_type: &'static str, payload: Value) -> NewEvent {
    NewEvent {
        event_id: EventId::new(),
        event_type,
        schema_version: 1,
        occurred_at: Utc::now(),
        correlation_id: Uuid::now_v7(),
        causation_id: Some(Uuid::now_v7()),
        actor: Actor::system("test"),
        payload,
    }
}

fn run_stream() -> StreamId {
    StreamId::new("run", Uuid::now_v7())
}

// ---------------------------------------------------------------------------
// (1) OCC conflict on concurrent appends to one stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_1_occ_conflict_on_concurrent_appends() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = run_stream();

    // Sequential baseline: version must match.
    let first = store
        .append(&stream, 0, &[event("run.started", json!({ "goal": "a" }))])
        .await
        .expect("first append");
    assert_eq!(first.new_version, 1);
    let err = store
        .append(&stream, 0, &[event("run.completed", json!({}))])
        .await
        .expect_err("stale expected version must conflict");
    match err {
        StoreError::VersionConflict {
            stream: s,
            expected,
            actual,
        } => {
            assert_eq!(s, stream);
            assert_eq!(expected, 0);
            assert_eq!(actual, 1);
        }
        other => panic!("expected VersionConflict, got {other:?}"),
    }

    // Concurrent: N writers all believe the stream is at version 1; exactly one wins.
    let writers = 8;
    let mut handles = Vec::new();
    for i in 0..writers {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store
                .append(
                    &stream,
                    1,
                    &[event("run.progressed", json!({ "writer": i }))],
                )
                .await
        }));
    }
    let mut ok = 0;
    let mut conflicts = 0;
    for h in handles {
        match h.await.expect("task") {
            Ok(res) => {
                ok += 1;
                assert_eq!(res.new_version, 2);
            }
            Err(StoreError::VersionConflict {
                expected, actual, ..
            }) => {
                conflicts += 1;
                assert_eq!(expected, 1);
                assert_eq!(actual, 2);
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
    assert_eq!(ok, 1, "exactly one concurrent append wins");
    assert_eq!(conflicts, writers - 1);

    let loaded = store.load_stream(&stream, 0).await.expect("load");
    assert_eq!(loaded.len(), 2);
    assert_eq!(
        loaded
            .iter()
            .map(|e| e.aggregate_version)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(store.stream_version(&stream).await.unwrap(), 2);

    // Empty appends are a caller bug, not a silent no-op.
    assert!(matches!(
        store.append(&stream, 2, &[]).await,
        Err(StoreError::EmptyAppend { .. })
    ));
    db.close().await;
}

// ---------------------------------------------------------------------------
// (2) Global position strictly increasing, gap-free under concurrent appends,
//     and a live catch-up reader never skips an event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_2_global_ordering_and_catch_up() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let writers = 6usize;
    let per_writer = 20usize;
    let total = writers * per_writer;

    // A reader that catches up by "everything > last seen" while writers run.
    let reader_store = store.clone();
    let stop = CancellationToken::new();
    let reader = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut last = 0u64;
            let mut seen: Vec<u64> = Vec::new();
            let mut ids: Vec<Uuid> = Vec::new();
            loop {
                let batch = reader_store.read_all(last, 7).await.expect("read_all");
                for e in &batch {
                    assert!(e.position > last, "positions must increase");
                    last = e.position;
                    seen.push(e.position);
                    ids.push(e.event_id.as_uuid());
                }
                if batch.is_empty() {
                    if stop.is_cancelled() {
                        // One final drain after writers finished.
                        let rest = reader_store.read_all(last, 1000).await.expect("read_all");
                        for e in rest {
                            seen.push(e.position);
                            ids.push(e.event_id.as_uuid());
                        }
                        return (seen, ids);
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        })
    };

    let mut handles = Vec::new();
    for w in 0..writers {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            let stream = StreamId::new("task", Uuid::now_v7());
            let mut ids = Vec::new();
            for v in 0..per_writer {
                let ev = event("task.progressed", json!({ "w": w, "v": v }));
                ids.push(ev.event_id.as_uuid());
                let res = store
                    .append(&stream, v as u64, &[ev])
                    .await
                    .expect("append");
                assert_eq!(res.new_version, v as u64 + 1);
                assert_eq!(res.first_position, res.last_position);
            }
            ids
        }));
    }
    let mut appended = BTreeSet::new();
    for h in handles {
        appended.extend(h.await.expect("writer"));
    }
    stop.cancel();
    let (seen, ids) = reader.await.expect("reader");

    // Strictly increasing and gap-free: positions 1..=total.
    let expected: Vec<u64> = (1..=total as u64).collect();
    assert_eq!(seen, expected, "positions must be contiguous 1..=n");
    // The live reader saw every event exactly once.
    let seen_ids: BTreeSet<Uuid> = ids.iter().copied().collect();
    assert_eq!(ids.len(), total, "no duplicates");
    assert_eq!(seen_ids, appended, "no event skipped");

    // Paging with read_all respects the limit and the exclusive lower bound.
    let page = store.read_all(0, 5).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    let page = store.read_all(5, 5).await.unwrap();
    assert_eq!(page.first().unwrap().position, 6);
    assert!(store.read_all(total as u64, 5).await.unwrap().is_empty());

    // The position watch ends at the head.
    let rx = store.subscribe_positions();
    assert_eq!(*rx.borrow(), total as u64);
    assert_eq!(store.head_position().await.unwrap(), total as u64);
    db.close().await;
}

// ---------------------------------------------------------------------------
// (3) Idempotent command replay returns the original result
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_3_idempotent_command_replay() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let log = CommandLog::new(db.pool().clone());
    let command_id = CommandId::new();
    let original = json!({ "run_id": Uuid::now_v7(), "status": "started" });

    let in_flight = match log.begin(command_id).await.unwrap() {
        Begun::Fresh(cmd) => cmd,
        Begun::Replayed(v) => panic!("fresh command replayed {v}"),
    };
    assert_eq!(in_flight.command_id(), command_id);
    assert_eq!(
        in_flight.complete(&original).await.unwrap(),
        CompleteOutcome::Recorded
    );

    // Replay: same command id → original result, no execution.
    match log.begin(command_id).await.unwrap() {
        Begun::Replayed(v) => assert_eq!(v, original),
        Begun::Fresh(_) => panic!("completed command must replay"),
    }
    // A late duplicate execution cannot overwrite the recorded result.
    assert_eq!(
        log.complete(command_id, &json!({ "status": "other" }))
            .await
            .unwrap(),
        CompleteOutcome::AlreadyRecorded(original.clone())
    );
    assert_eq!(log.result_of(command_id).await.unwrap(), Some(original));
    // Other commands are unaffected.
    assert!(matches!(
        log.begin(CommandId::new()).await.unwrap(),
        Begun::Fresh(_)
    ));
    db.close().await;
}

// ---------------------------------------------------------------------------
// (4) Outbox rows delivered exactly once to the in-proc relay under crash
//     simulation (kill between commit and relay; kill during delivery)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_4_outbox_exactly_once_under_crash() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = run_stream();

    // Process 1 commits three events and "dies" before its relay runs.
    let res = store
        .append(
            &stream,
            0,
            &[
                event("run.started", json!({ "n": 1 })),
                event("run.progressed", json!({ "n": 2 })),
                event("run.progressed", json!({ "n": 3 })),
            ],
        )
        .await
        .unwrap();
    drop(store);
    let outbox = Outbox::new(db.pool().clone());
    assert_eq!(outbox.pending_count().await.unwrap(), 3);

    // Process 2 (restart) relays: every row delivered exactly once, in order.
    let delivered: Arc<std::sync::Mutex<Vec<u64>>> = Arc::default();
    let restart_pool = db.connect_other().await;
    let outbox2 = Outbox::new(restart_pool.clone()).batch_size(2);
    let sink = delivered.clone();
    let report = outbox2
        .drain(move |batch| {
            let sink = sink.clone();
            async move {
                sink.lock()
                    .unwrap()
                    .extend(batch.iter().map(|e| e.position));
                Ok(())
            }
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.delivered, 3);
    assert_eq!(report.last_position, res.last_position);
    assert_eq!(
        *delivered.lock().unwrap(),
        (res.first_position..=res.last_position).collect::<Vec<_>>()
    );
    assert_eq!(outbox2.pending_count().await.unwrap(), 0);
    // Nothing is delivered twice.
    let again = outbox2
        .relay_once(|_| async { Ok(()) })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.delivered, 0);

    // Crash *during* delivery: the handler fails after the event is committed.
    let store = PgEventStore::new(db.pool().clone());
    store
        .append(&stream, 3, &[event("run.completed", json!({}))])
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let failed = outbox2
        .relay_once(move |_batch| {
            c.fetch_add(1, Ordering::SeqCst);
            async { Err(DeliveryError::new("simulated crash")) }
        })
        .await
        .unwrap();
    assert!(failed.is_err());
    assert_eq!(
        outbox2.pending_count().await.unwrap(),
        1,
        "row stays pending"
    );
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempts FROM core.outbox WHERE delivered_at IS NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(attempts, 1);
    // Next pass delivers it once; afterwards nothing is pending.
    let c = calls.clone();
    let ok = outbox2
        .relay_once(move |batch| {
            c.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(batch.len(), 1);
                assert_eq!(batch[0].event_type, "run.completed");
                Ok(())
            }
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ok.delivered, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(outbox2.pending_count().await.unwrap(), 0);

    // The long-running relay wakes on the store's position watch.
    let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
    let cancel = CancellationToken::new();
    let relay = {
        let outbox = Outbox::new(db.pool().clone()).poll_interval(Duration::from_secs(30));
        let wake = store.subscribe_positions();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            outbox
                .relay(
                    move |batch| {
                        let tx = tx.clone();
                        async move {
                            for e in batch {
                                tx.send(e.position)
                                    .map_err(|e| DeliveryError::new(e.to_string()))?;
                            }
                            Ok(())
                        }
                    },
                    wake,
                    cancel,
                )
                .await
        })
    };
    let res = store
        .append(&stream, 4, &[event("run.evaluated", json!({}))])
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("relay delivers within 5s")
        .expect("channel open");
    assert_eq!(got, res.last_position);
    cancel.cancel();
    let report = relay.await.unwrap().unwrap();
    assert_eq!(report.delivered, 1);
    restart_pool.close().await;
    db.close().await;
}

// ---------------------------------------------------------------------------
// (5) Migrations idempotent (store half; CLI half in kevin-cli tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_5_migrations_idempotent() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let pool = db.pool();

    // The template already applied everything: a second run applies nothing.
    let report = migrate::migrate(pool, MigratePolicy::Apply).await.unwrap();
    assert!(report.applied.is_empty(), "second migrate must be a no-op");
    assert!(report.already_applied.contains(&1));
    let status = migrate::status(pool).await.unwrap();
    assert!(status.is_current());
    assert!(
        status.pgvector_installed,
        "0001_core creates the vector extension"
    );
    assert!(
        status
            .entries
            .iter()
            .all(|e| e.state == MigrationState::Applied)
    );
    // CheckOnly passes on a current database.
    migrate::migrate(pool, MigratePolicy::CheckOnly)
        .await
        .unwrap();

    // reset drops the core schema and re-applies: tables exist again and are empty.
    let store = PgEventStore::new(pool.clone());
    store
        .append(&run_stream(), 0, &[event("run.started", json!({}))])
        .await
        .unwrap();
    // Every embedded migration is re-applied (WS-11 added `0002_orch`, and
    // later workstreams add more), so compare against the embedded set.
    let embedded: Vec<i64> = kevin_store::MIGRATOR.iter().map(|m| m.version).collect();
    let report = migrate::reset(pool).await.unwrap();
    assert_eq!(report.applied, embedded);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM core.events")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(migrate::status(pool).await.unwrap().is_current());

    // A database without migrations: CheckOnly reports what is pending.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("DROP SCHEMA core CASCADE")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DROP TABLE public._sqlx_migrations")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    match migrate::migrate(pool, MigratePolicy::CheckOnly).await {
        Err(StoreError::MigrationsPending { pending }) => assert_eq!(pending, embedded),
        other => panic!("expected MigrationsPending, got {other:?}"),
    }
    let report = migrate::migrate(pool, MigratePolicy::Apply).await.unwrap();
    assert_eq!(report.applied, embedded);
    db.close().await;
}

// ---------------------------------------------------------------------------
// (6) Upcaster applies on load for a v1→v2 fixture
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws03_6_upcaster_applies_on_load() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let raw = PgEventStore::new(db.pool().clone());
    let stream = run_stream();
    // v1 fixture: `goal` is a plain string; v2 wraps it and adds `mode`.
    raw.append(
        &stream,
        0,
        &[event("run.started", json!({ "goal": "add /healthz" }))],
    )
    .await
    .unwrap();

    let upcasters = Upcasters::new().with("run.started", 1, |mut payload| {
        let goal = payload["goal"].take();
        json!({ "goal": { "text": goal, "attachments": [] }, "mode": "interactive" })
    });
    let store = PgEventStore::with_upcasters(db.pool().clone(), upcasters.clone());

    let loaded = store.load_stream(&stream, 0).await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].schema_version, 2);
    assert_eq!(
        loaded[0].payload,
        json!({ "goal": { "text": "add /healthz", "attachments": [] }, "mode": "interactive" })
    );
    let all = store.read_all(0, 10).await.unwrap();
    assert_eq!(all[0].schema_version, 2);
    assert_eq!(all[0].payload["mode"], json!("interactive"));

    // The stored row is untouched: a store without upcasters still sees v1.
    let as_stored = raw.load_stream(&stream, 0).await.unwrap();
    assert_eq!(as_stored[0].schema_version, 1);
    assert_eq!(as_stored[0].payload, json!({ "goal": "add /healthz" }));

    // The outbox relay applies the same registry.
    let outbox = Outbox::new(db.pool().clone()).with_upcasters(upcasters);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    outbox
        .drain(move |batch| {
            let s = s.clone();
            async move {
                s.lock()
                    .unwrap()
                    .extend(batch.into_iter().map(|e| e.schema_version));
                Ok(())
            }
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*seen.lock().unwrap(), vec![2]);
    db.close().await;
}

// ---------------------------------------------------------------------------
// Supporting behaviour: NOTIFY, snapshots, checkpoints, stream reads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn append_notifies_kevin_events_with_last_position() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let mut listener = PgListener::connect_with(db.pool()).await.unwrap();
    listener.listen(NOTIFY_CHANNEL).await.unwrap();

    let res = store
        .append(
            &run_stream(),
            0,
            &[
                event("run.started", json!({})),
                event("run.progressed", json!({})),
            ],
        )
        .await
        .unwrap();
    let note = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .expect("notification within 5s")
        .unwrap();
    assert_eq!(note.channel(), NOTIFY_CHANNEL);
    assert_eq!(
        kevin_store::event_store::parse_notify_payload(note.payload()),
        Some(res.last_position)
    );
    // The listener holds a pool connection; release it before closing the pool.
    drop(listener);
    db.close().await;
}

#[tokio::test]
async fn snapshots_and_checkpoints_round_trip() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let stream = run_stream();
    let snaps = Snapshots::new(db.pool().clone());
    assert!(snaps.load(&stream).await.unwrap().is_none());
    snaps
        .save(&stream, 3, &json!({ "state": "planning" }))
        .await
        .unwrap();
    let snap = snaps.load(&stream).await.unwrap().unwrap();
    assert_eq!(snap.version, 3);
    assert_eq!(snap.state, json!({ "state": "planning" }));
    // Never moves backwards.
    snaps
        .save(&stream, 2, &json!({ "state": "older" }))
        .await
        .unwrap();
    assert_eq!(snaps.load(&stream).await.unwrap().unwrap().version, 3);
    snaps
        .save(&stream, 5, &json!({ "state": "executing" }))
        .await
        .unwrap();
    assert_eq!(snaps.load(&stream).await.unwrap().unwrap().version, 5);
    assert!(snaps.delete(&stream).await.unwrap());
    assert!(snaps.load(&stream).await.unwrap().is_none());

    let cps = Checkpoints::new(db.pool().clone());
    assert_eq!(cps.get("task_board").await.unwrap(), None);
    cps.set("task_board", 10).await.unwrap();
    cps.set("task_board", 42).await.unwrap();
    cps.set("run_overview", 7).await.unwrap();
    assert_eq!(cps.get("task_board").await.unwrap(), Some(42));
    assert_eq!(
        cps.list().await.unwrap(),
        vec![
            ("run_overview".to_owned(), 7),
            ("task_board".to_owned(), 42)
        ]
    );
    assert!(cps.delete("task_board").await.unwrap());
    assert!(!cps.delete("task_board").await.unwrap());
    db.close().await;
}

#[tokio::test]
async fn load_stream_from_version_is_exclusive_and_envelopes_round_trip() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let stream = StreamId::new("question", Uuid::now_v7());
    let mut ev = event("question.asked", json!({ "text": "which db?" }));
    ev.actor = Actor::user("valentin");
    ev.schema_version = 3;
    let res = store
        .append(
            &stream,
            0,
            &[
                ev.clone(),
                event("question.answered", json!({ "answer": "pg" })),
                event("question.expired", json!({ "applied_default": false })),
            ],
        )
        .await
        .unwrap();
    assert_eq!(res.events.len(), 3);
    assert_eq!(res.events[0].aggregate_version, 1);
    assert_eq!(res.events[0].stream(), stream);

    let tail = store.load_stream(&stream, 1).await.unwrap();
    assert_eq!(
        tail.iter().map(|e| e.event_type).collect::<Vec<_>>(),
        vec!["question.answered", "question.expired"]
    );
    let full = store.load_stream(&stream, 0).await.unwrap();
    let first = &full[0].envelope;
    assert_eq!(first.event_id, ev.event_id);
    assert_eq!(first.event_type, "question.asked");
    assert_eq!(first.schema_version, 3);
    assert_eq!(first.aggregate_type, "question");
    assert_eq!(first.aggregate_id, stream.aggregate_id);
    assert_eq!(first.correlation_id, ev.correlation_id);
    assert_eq!(first.causation_id, ev.causation_id);
    assert_eq!(first.actor, Actor::user("valentin"));
    assert_eq!(first.payload, json!({ "text": "which db?" }));
    assert_eq!(
        first.occurred_at.timestamp_micros(),
        ev.occurred_at.timestamp_micros()
    );
    assert!(store.load_stream(&stream, 3).await.unwrap().is_empty());
    assert!(
        store
            .load_stream(&StreamId::new("question", Uuid::now_v7()), 0)
            .await
            .unwrap()
            .is_empty()
    );
    db.close().await;
}

#[tokio::test]
async fn test_databases_are_isolated() {
    kevin_testkit::skip_unless_pg!();
    let a = TestDb::new().await;
    let b = TestDb::new().await;
    assert_ne!(a.name(), b.name());
    PgEventStore::new(a.pool().clone())
        .append(&run_stream(), 0, &[event("run.started", json!({}))])
        .await
        .unwrap();
    let in_b: i64 = sqlx::query_scalar("SELECT count(*) FROM core.events")
        .fetch_one(b.pool())
        .await
        .unwrap();
    assert_eq!(in_b, 0);
    a.close().await;
    b.close().await;
}
