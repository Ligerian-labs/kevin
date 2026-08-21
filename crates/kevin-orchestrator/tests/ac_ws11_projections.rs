//! WS-11 acceptance criteria (`plan/12-workstreams.md` §WS-11) against a real
//! Postgres (`kevin_testkit::pg::TestDb`, one database per test):
//!
//! 1. replaying the same events twice yields identical tables;
//! 2. `rebuild` from scratch equals the incremental apply;
//! 3. the projection lag metric is exposed;
//! 4. `orch.task_log` appends are monotonic per attempt.

use std::sync::Arc;

use kevin_domain::aggregate::EventMeta;
use kevin_domain::ids::{EventId, TaskId};
use kevin_domain::kinds::FailureClass;
use kevin_domain::values::Usage;
use kevin_domain::{Actor, DomainEvent};
use kevin_orchestrator::projections::{
    self, CostGroupBy, CostQuery, NewTaskLogLine, ProjectionRunner, QuestionQuery, ReadModels,
    RunQuery, TaskLog, TaskLogQuery, TaskQuery,
};
use kevin_store::{EventStore, NewEvent, PgEventStore, PgPool, StreamId};
use kevin_testkit::given_when_then::{ids, question, run, task};
use kevin_testkit::pg::TestDb;
use serde_json::Value;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Fixture: one complete run (understanding → question → plan → two tasks with a
// retry → integration → evaluation → completion).
// ---------------------------------------------------------------------------

/// One event to append, with the stream it belongs to.
struct Fixture {
    aggregate_type: &'static str,
    aggregate_id: Uuid,
    event: DomainEvent,
}

fn on(aggregate_type: &'static str, aggregate_id: Uuid, event: impl Into<DomainEvent>) -> Fixture {
    Fixture {
        aggregate_type,
        aggregate_id,
        event: event.into(),
    }
}

/// The full scenario, in the order the orchestrator would emit it.
fn scenario() -> Vec<Fixture> {
    let run_id = ids::run_id().as_uuid();
    let question_id = ids::question_id(1).as_uuid();
    let task_1 = ids::task_id(1).as_uuid();
    let task_2 = ids::task_id(2).as_uuid();
    let usage = kevin_testkit::given_when_then::values::usage();

    vec![
        on("run", run_id, run::started()),
        on("run", run_id, run::understanding_started()),
        on(
            "run",
            run_id,
            run::understanding_completed_with_questions(vec![ids::question_id(1)]),
        ),
        on("question", question_id, question::asked()),
        on("question", question_id, question::answered()),
        on(
            "run",
            run_id,
            run::question_answered(ids::question_id(1), 0),
        ),
        on("run", run_id, run::plan_proposed()),
        on("run", run_id, run::plan_approved()),
        on("run", run_id, run::execution_started()),
        on("task", task_1, task::created()),
        on("task", task_1, task::routed()),
        on("task", task_1, task::attempt_started(1)),
        on("task", task_1, task::progressed(1)),
        on(
            "task",
            task_1,
            task::attempt_failed(1, FailureClass::Transient, true),
        ),
        on("task", task_1, task::retried(2)),
        on("task", task_1, task::rerouted()),
        on("task", task_1, task::attempt_started(2)),
        on("task", task_1, task::attempt_succeeded(2)),
        on(
            "run",
            run_id,
            run::task_terminal_noted(ids::task_id(1), false, usage),
        ),
        on("task", task_2, task_two_created()),
        on("task", task_2, task::routed()),
        on("task", task_2, task::attempt_started(3)),
        on("task", task_2, task::attempt_succeeded(3)),
        on(
            "run",
            run_id,
            run::task_terminal_noted(ids::task_id(2), true, usage + usage),
        ),
        on("run", run_id, run::integrated()),
        on("run", run_id, run::evaluated()),
        on("run", run_id, run::completed(usage + usage)),
    ]
}

/// `task.created` for the second plan task.
fn task_two_created() -> kevin_domain::task::TaskEvent {
    let kevin_domain::task::TaskEvent::Created {
        kind, spec, budget, ..
    } = task::created()
    else {
        unreachable!("task::created is TaskEvent::Created")
    };
    let mut spec = spec;
    "Test /healthz".clone_into(&mut spec.title);
    spec.depends_on = vec![ids::task_id(1)];
    kevin_domain::task::TaskEvent::Created {
        task_id: ids::task_id(2),
        run_id: ids::run_id(),
        kind,
        spec,
        budget,
    }
}

/// Appends every fixture event to its stream, in order.
async fn append_scenario(store: &PgEventStore, fixtures: &[Fixture]) {
    let run_id = ids::run_id().as_uuid();
    for fixture in fixtures {
        let stream = StreamId::new(fixture.aggregate_type, fixture.aggregate_id);
        let version = store.stream_version(&stream).await.expect("stream version");
        let event = NewEvent {
            event_id: EventId::new(),
            event_type: fixture.event.event_type(),
            schema_version: fixture.event.schema_version(),
            occurred_at: kevin_testkit::given_when_then::fixture_time()
                + chrono::TimeDelta::seconds(i64::try_from(version).unwrap_or(0)),
            correlation_id: run_id,
            causation_id: None,
            actor: Actor::system("test"),
            payload: serde_json::to_value(&fixture.event).expect("payload"),
        };
        store
            .append(&stream, version, std::slice::from_ref(&event))
            .await
            .expect("append");
    }
}

/// Every `orch` table dumped as sorted JSON, for byte comparisons.
async fn snapshot(pool: &PgPool) -> Vec<(String, Value)> {
    let tables = [
        ("run_overview", "run_id"),
        ("task_board", "task_id"),
        ("question_inbox", "question_id"),
        ("cost_ledger", "attempt_id"),
        ("task_log", "task_id, attempt, seq"),
        ("artifacts", "artifact_id"),
    ];
    let mut out = Vec::new();
    for (table, order) in tables {
        let rows: Option<Value> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT jsonb_agg(to_jsonb(t) ORDER BY {order}) FROM orch.{table} t"
        )))
        .fetch_one(pool)
        .await
        .expect("snapshot");
        out.push((table.to_owned(), rows.unwrap_or(Value::Null)));
    }
    out
}

/// A runner per projection, all caught up from their checkpoints.
async fn catch_up_all(pool: &PgPool, store: &Arc<dyn EventStore>) -> u64 {
    let mut applied = 0;
    for projection in projections::all() {
        let mut runner = ProjectionRunner::new(projection, pool.clone(), Arc::clone(store));
        runner.load_checkpoint().await.expect("checkpoint");
        applied += runner.catch_up().await.expect("catch up");
    }
    applied
}

fn erased(store: &PgEventStore) -> Arc<dyn EventStore> {
    Arc::new(store.clone())
}

// ---------------------------------------------------------------------------
// (1) replaying the same events twice yields identical tables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws11_1_replay_twice_yields_identical_tables() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store, &scenario()).await;

    let applied = catch_up_all(db.pool(), &erased(&store)).await;
    assert!(applied > 0, "the first pass must apply events");
    let first = snapshot(db.pool()).await;

    // Rewind every checkpoint and replay the same events over the same rows.
    let checkpoints = kevin_store::Checkpoints::new(db.pool().clone());
    for name in projections::NAMES {
        checkpoints.delete(name).await.expect("delete checkpoint");
    }
    let replayed = catch_up_all(db.pool(), &erased(&store)).await;
    assert_eq!(replayed, applied, "the replay must see the same events");
    let second = snapshot(db.pool()).await;

    for ((table, before), (_, after)) in first.iter().zip(second.iter()) {
        assert_eq!(before, after, "orch.{table} changed on replay");
    }

    // Spot-check the projected state itself, not only its stability.
    let read = ReadModels::new(db.pool().clone());
    let run = read
        .run(ids::run_id().as_uuid())
        .await
        .expect("query")
        .expect("the run is projected");
    assert_eq!(run.status, "completed");
    assert_eq!(run.tasks_total, 2);
    assert_eq!(run.tasks_succeeded, 2);
    assert!(run.open_question_ids.is_empty());
    assert_eq!(run.plan_revision, 0);
    assert!(run.understanding.is_some() && run.plan.is_some());
    assert_eq!(run.evaluation_verdict.as_deref(), Some("accept"));

    let tasks = read
        .tasks_of_run(ids::run_id().as_uuid())
        .await
        .expect("tasks");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].status, "succeeded");
    assert_eq!(tasks[0].attempt_count, 2, "task 1 was retried once");
    let attempts = tasks[0].attempts.as_array().expect("attempts array");
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0]["status"], "failed");
    assert_eq!(attempts[1]["status"], "succeeded");

    let questions = read
        .questions(&QuestionQuery {
            status: Some("answered".to_owned()),
            ..QuestionQuery::default()
        })
        .await
        .expect("questions");
    assert_eq!(questions.len(), 1);
    assert_eq!(questions.items[0].answered_by.as_deref(), Some("valentin"));

    let cost = read
        .cost(&CostQuery {
            group_by: CostGroupBy::Model,
            ..CostQuery::default()
        })
        .await
        .expect("cost");
    assert_eq!(
        cost.rows.iter().map(|r| r.attempts).sum::<i64>(),
        3,
        "three attempts are on the ledger"
    );
    assert!(cost.total_usd.is_some_and(|usd| usd > 0.into()));

    let artifacts = read
        .artifacts_of_run(ids::run_id().as_uuid())
        .await
        .expect("artifacts");
    assert_eq!(artifacts.len(), 1, "the fixture reuses one artifact id");

    db.close().await;
}

// ---------------------------------------------------------------------------
// (2) rebuild from scratch equals the incremental apply
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws11_2_rebuild_equals_incremental() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let erased = erased(&store);

    // Incremental: apply after every append, one event at a time.
    for fixture in scenario() {
        append_scenario(&store, std::slice::from_ref(&fixture)).await;
        catch_up_all(db.pool(), &erased).await;
    }
    let incremental = snapshot(db.pool()).await;

    // Rebuild: truncate and replay from position 0.
    let reports = projections::rebuild_all(db.pool().clone(), Arc::clone(&erased))
        .await
        .expect("rebuild");
    assert_eq!(reports.len(), projections::NAMES.len());
    assert!(
        reports.iter().all(|r| r.events > 0 && r.position > 0),
        "every projection replayed events: {reports:?}"
    );
    let rebuilt = snapshot(db.pool()).await;

    for ((table, incremental), (_, rebuilt)) in incremental.iter().zip(rebuilt.iter()) {
        assert_eq!(
            incremental, rebuilt,
            "orch.{table} differs between incremental and rebuild"
        );
    }

    // A single projection can be rebuilt on its own.
    let report = projections::rebuild(db.pool().clone(), erased, "task_board")
        .await
        .expect("rebuild one");
    assert_eq!(report.name, "task_board");
    let after_one = snapshot(db.pool()).await;
    assert_eq!(incremental, after_one);

    // An unknown name is rejected, not silently ignored.
    let store2: Arc<dyn EventStore> = Arc::new(PgEventStore::new(db.pool().clone()));
    let err = projections::rebuild(db.pool().clone(), store2, "nope")
        .await
        .expect_err("unknown projection");
    assert!(err.to_string().contains("unknown projection"), "{err}");

    db.close().await;
}

// ---------------------------------------------------------------------------
// (3) the lag metric is exposed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws11_3_lag_metric_exposed() {
    kevin_testkit::skip_unless_pg!();
    let metrics = kevin_telemetry::metrics::install().expect("prometheus recorder");
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store, &scenario()).await;

    let mut runner = ProjectionRunner::new(
        projections::by_name("run_overview").expect("projection"),
        db.pool().clone(),
        erased(&store),
    );
    runner.load_checkpoint().await.expect("checkpoint");
    let lag = runner.record_lag().await.expect("lag");
    assert!(lag > 0, "an unstarted projection lags behind the store");

    let rendered = metrics.render();
    assert!(
        rendered.contains("kevin_projection_lag_events"),
        "the lag gauge is exposed:\n{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "kevin_projection_lag_events{{projection=\"run_overview\"}} {lag}"
        )),
        "the gauge is labelled by projection and carries the lag:\n{rendered}"
    );

    // Catching up brings the lag back to zero.
    runner.catch_up().await.expect("catch up");
    assert_eq!(runner.record_lag().await.expect("lag"), 0);
    let rendered = metrics.render();
    assert!(
        rendered.contains("kevin_projection_lag_events{projection=\"run_overview\"} 0"),
        "the gauge is updated after catch-up:\n{rendered}"
    );
    assert!(
        rendered.contains("kevin_projection_apply_duration_seconds"),
        "apply latency is exposed too:\n{rendered}"
    );

    db.close().await;
}

// ---------------------------------------------------------------------------
// (4) task_log append is monotonic per attempt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws11_4_task_log_append_is_monotonic_per_attempt() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store, &scenario()).await;
    catch_up_all(db.pool(), &erased(&store)).await;

    let read = ReadModels::new(db.pool().clone());
    let task_id = ids::task_id(1).as_uuid();

    // Worker lines interleave with the lifecycle lines the projection wrote,
    // from several writers at once.
    let log = TaskLog::new(db.pool().clone());
    let mut writers = Vec::new();
    for writer in 0..4u32 {
        let log = log.clone();
        writers.push(tokio::spawn(async move {
            for line in 0..10u32 {
                let line = NewTaskLogLine::new(
                    TaskId::from_uuid(task_id),
                    2,
                    "assistant",
                    serde_json::json!({ "writer": writer, "line": line }),
                );
                log.append(&line).await.expect("append");
            }
        }));
    }
    for writer in writers {
        writer.await.expect("writer");
    }

    // Per attempt: strictly increasing, gap-free, starting at 1.
    for attempt in [0, 1, 2] {
        let page = read
            .task_log(&TaskLogQuery {
                task_id,
                attempt: Some(attempt),
                after_seq: None,
                limit: Some(500),
            })
            .await
            .expect("task log");
        let seqs: Vec<i64> = page.items.iter().map(|row| row.seq).collect();
        let expected: Vec<i64> = (1..=i64::try_from(seqs.len()).unwrap()).collect();
        assert_eq!(
            seqs, expected,
            "attempt {attempt} must be 1..n without gaps"
        );
        assert!(
            page.items.iter().all(|row| row.attempt == attempt),
            "attempt {attempt} page is filtered"
        );
    }

    let attempt_2 = read.task_log_head(task_id, 2).await.expect("head");
    assert_eq!(attempt_2, 40 + 2, "40 worker lines + started + succeeded");

    // `after_seq` pages forward without repeating a line.
    let page = read
        .task_log(&TaskLogQuery {
            task_id,
            attempt: Some(2),
            after_seq: Some(0),
            limit: Some(5),
        })
        .await
        .expect("first page");
    assert_eq!(page.len(), 5);
    assert_eq!(page.next_cursor.as_deref(), Some("5"));
    let next = read
        .task_log(&TaskLogQuery {
            task_id,
            attempt: Some(2),
            after_seq: Some(5),
            limit: Some(5),
        })
        .await
        .expect("second page");
    assert_eq!(next.items.first().map(|row| row.seq), Some(6));

    // Replaying the projection adds no line and consumes no seq.
    let before = read.task_log_head(task_id, 2).await.expect("head");
    let checkpoints = kevin_store::Checkpoints::new(db.pool().clone());
    checkpoints.delete("task_log").await.expect("checkpoint");
    let mut runner = ProjectionRunner::new(
        projections::by_name("task_log").expect("projection"),
        db.pool().clone(),
        erased(&store),
    );
    runner.load_checkpoint().await.expect("checkpoint");
    runner.catch_up().await.expect("catch up");
    assert_eq!(
        read.task_log_head(task_id, 2).await.expect("head"),
        before,
        "replaying task.* must not duplicate lifecycle lines"
    );

    // A rebuild drops the projection's own lines and re-appends them after the
    // worker transcript, which it must never touch. `seq` keeps increasing.
    projections::rebuild(db.pool().clone(), erased(&store), "task_log")
        .await
        .expect("rebuild");
    let lines = read
        .task_log(&TaskLogQuery {
            task_id,
            attempt: Some(2),
            after_seq: None,
            limit: Some(500),
        })
        .await
        .expect("task log")
        .items;
    assert_eq!(
        lines.iter().filter(|row| row.kind == "assistant").count(),
        40,
        "worker lines survive a rebuild"
    );
    assert_eq!(
        lines.iter().filter(|row| row.kind == "system").count(),
        2,
        "the two lifecycle lines are written exactly once"
    );
    assert!(
        lines.windows(2).all(|w| w[0].seq < w[1].seq),
        "seq stays strictly monotonic across a rebuild"
    );
    assert!(
        read.task_log_head(task_id, 2).await.expect("head") >= before,
        "a rebuild never rewinds the head"
    );

    db.close().await;
}

// ---------------------------------------------------------------------------
// Read-model query surface used by the API and the CLI
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_models_paginate_and_filter() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    append_scenario(&store, &scenario()).await;
    catch_up_all(db.pool(), &erased(&store)).await;
    let read = ReadModels::new(db.pool().clone());

    let page = read
        .runs(&RunQuery {
            limit: Some(1),
            ..RunQuery::default()
        })
        .await
        .expect("runs");
    assert_eq!(page.len(), 1);
    let cursor = page.next_cursor.clone().expect("a full page has a cursor");
    let next = read
        .runs(&RunQuery {
            cursor: Some(cursor),
            ..RunQuery::default()
        })
        .await
        .expect("second page");
    assert!(next.is_empty(), "only one run exists");

    let filtered = read
        .runs(&RunQuery {
            status: Some("failed".to_owned()),
            ..RunQuery::default()
        })
        .await
        .expect("filtered");
    assert!(filtered.is_empty());

    let tasks = read
        .tasks(&TaskQuery {
            run_id: Some(ids::run_id().as_uuid()),
            status: Some("succeeded".to_owned()),
            ..TaskQuery::default()
        })
        .await
        .expect("tasks");
    assert_eq!(tasks.len(), 2);

    let by_run = read
        .cost(&CostQuery {
            group_by: CostGroupBy::Run,
            run_id: Some(ids::run_id().as_uuid()),
            since: None,
        })
        .await
        .expect("cost by run");
    assert_eq!(by_run.rows.len(), 1);
    assert_eq!(by_run.rows[0].key, ids::run_id().as_uuid().to_string());
    assert!(by_run.total_tokens > 0);

    let entries = read
        .cost_entries(&CostQuery::default())
        .await
        .expect("entries");
    assert_eq!(entries.len(), 3);
    assert!(entries.iter().all(|e| e.task_kind == "implement"));

    let usage: Usage = serde_json::from_value(
        read.task(ids::task_id(1).as_uuid())
            .await
            .unwrap()
            .unwrap()
            .usage,
    )
    .expect("task usage round-trips as a domain Usage");
    assert!(usage.total_tokens() > 0);

    db.close().await;
}

// ---------------------------------------------------------------------------
// The runner follows the bus live and stops on cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runner_follows_the_bus_and_stops_on_cancel() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = PgEventStore::new(db.pool().clone());
    let bus: Arc<dyn kevin_bus::EventBus> = Arc::new(kevin_bus::InProcBus::with_defaults());
    let read = ReadModels::new(db.pool().clone());
    let fixtures = scenario();
    let (before, after) = fixtures.split_at(3);

    // Everything that happened before the projection started.
    append_scenario(&store, before).await;
    publish(&store, &bus, 0).await;

    let cancel = tokio_util::sync::CancellationToken::new();
    let runner = ProjectionRunner::new(
        projections::by_name("run_overview").expect("projection"),
        db.pool().clone(),
        erased(&store),
    );
    let handle = tokio::spawn({
        let bus = Arc::clone(&bus);
        let cancel = cancel.clone();
        async move { runner.run(bus, cancel).await }
    });

    // …and everything that happens while it runs, one event at a time.
    let start = u64::try_from(before.len()).unwrap_or(0);
    for (published, fixture) in (start..).zip(after.iter()) {
        append_scenario(&store, std::slice::from_ref(fixture)).await;
        publish(&store, &bus, published).await;
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let row = read.run(ids::run_id().as_uuid()).await.expect("query");
        if row.is_some_and(|row| row.status == "completed") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "run never completed");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    cancel.cancel();
    handle
        .await
        .expect("runner task")
        .expect("runner stopped cleanly");

    db.close().await;
}

/// Publishes every stored event after `from` on the bus, in position order, so
/// bus positions match store positions.
async fn publish(store: &PgEventStore, bus: &Arc<dyn kevin_bus::EventBus>, from: u64) {
    let events = store.read_all(from, 128).await.expect("read");
    let envelopes: Vec<kevin_bus::Event> = events.into_iter().map(|e| e.envelope).collect();
    bus.publish(&envelopes).await.expect("publish");
}
