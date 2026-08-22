//! WS-25 hardening — failure injection against a live engine (`plan/12` §WS-25).
//!
//! The WS-08 scenarios prove the saga does the right thing when everything
//! works. These prove it does something *sane* when the machine underneath it
//! does not: a runtime killed mid-attempt, a Postgres that goes away, a
//! `data_dir` that cannot be written, a worker binary that disappears, and a
//! subscriber too slow for the bus.
//!
//! Each one asserts three things in some form: the run reaches a terminal
//! state, the event stream stays consistent (no half-written attempt, no
//! replay of work that was already done), and the operator gets a message that
//! names the real cause.
//!
//! All of them need Postgres and skip cleanly without it.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{Harness, HoldOnce, Setup, assert_order, count, plan_of, understanding};
use kevin_bus::{EventBus, InProcBus, InProcBusConfig};
use kevin_domain::{ModelAlias, RunMode, WorkerKind};
use kevin_orchestrator::projections::{ProjectionRunner, by_name};
use kevin_orchestrator::testing::ScriptedRoles;
use kevin_store::{EventStore, PgEventStore, StoredEvent};
use kevin_testkit::pg::TestDb;
use kevin_worker::fake::{FakeWorker, Rule, Scenario};
use kevin_worker::{Worker, WorkerError};
use tokio_util::sync::CancellationToken;

fn types(events: &[StoredEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.envelope.event_type).collect()
}

fn payloads<'a>(events: &'a [StoredEvent], event_type: &str) -> Vec<&'a serde_json::Value> {
    events
        .iter()
        .filter(|e| e.envelope.event_type == event_type)
        .map(|e| &e.envelope.payload)
        .collect()
}

// ---------------------------------------------------------------------------
// (1) An abrupt restart terminalises the attempt and never replays it
// ---------------------------------------------------------------------------

/// The in-process half of the kill −9 scenario: the engine dies with an attempt
/// in flight and a fresh engine boots over the same store.
///
/// `ac_ws08_15` already asserts the happy shape of that sequence. What it does
/// *not* assert — and what a crash would actually corrupt — is that the killed
/// attempt is never re-executed: the same `attempt_id` must not be handed to a
/// worker twice, and the events written before the crash must not be appended
/// again. The out-of-process half (a real `kill -9` on the `kevin` binary) is
/// `ac_ws25_1_2` in `kevin-cli`.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_1_1_restart_terminalises_the_attempt_and_never_replays_it() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("survive a kill"))
            .with_plan(plan_of(2)),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let inner: Arc<dyn Worker> = Arc::new(FakeWorker::new(
        Scenario::replying("done").with_default(Rule::default().hold()),
        dir.path(),
    ));
    let holding = Arc::new(HoldOnce::new(inner));
    let mut harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .worker(Arc::clone(&holding) as Arc<dyn Worker>),
    )
    .await;
    let run = harness.start("survive a kill", RunMode::Headless).await;
    harness.wait_for_n(run, "task.attempt_started", 2).await;
    common::eventually("both held attempts to reach the worker", || {
        holding.started() == 2
    })
    .await;

    let before = harness.events(run).await;
    let killed_attempts: Vec<String> = payloads(&before, "task.attempt_started")
        .iter()
        .map(|p| p["attempt_id"].to_string())
        .collect();
    assert_eq!(killed_attempts.len(), 2);

    harness.crash().await;
    harness.reboot().await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    // Every attempt that was in flight is terminalised, with the contractual
    // class — not left `running` forever, and not resumed.
    let restarted: Vec<&serde_json::Value> = payloads(&events, "task.attempt_failed")
        .into_iter()
        .filter(|p| p["class"] == "runtime_restarted")
        .collect();
    assert_eq!(
        restarted.len(),
        2,
        "both in-flight attempts must be terminalised: {seen:?}"
    );

    // No replay: an attempt id appears in exactly one `task.attempt_started`,
    // and the two killed ids are never started again.
    let started: Vec<String> = payloads(&events, "task.attempt_started")
        .iter()
        .map(|p| p["attempt_id"].to_string())
        .collect();
    let unique: std::collections::BTreeSet<&String> = started.iter().collect();
    assert_eq!(
        unique.len(),
        started.len(),
        "an attempt id was started twice: {started:?}"
    );
    for killed in &killed_attempts {
        assert_eq!(
            started.iter().filter(|id| *id == killed).count(),
            1,
            "the killed attempt {killed} was replayed"
        );
    }

    // Events written before the crash are still there exactly once: the
    // rebooted engine appended after them instead of rewriting the stream.
    let before_positions: Vec<u64> = before.iter().map(|e| e.position).collect();
    let after_positions: Vec<u64> = events.iter().map(|e| e.position).collect();
    assert!(
        after_positions.starts_with(&before_positions),
        "the reboot rewrote history: {before_positions:?} vs {after_positions:?}"
    );

    assert_order(&seen, &["task.attempt_started", "task.attempt_failed"]);
    harness.shutdown().await;
}

// ---------------------------------------------------------------------------
// (3) An unwritable data_dir
// ---------------------------------------------------------------------------

/// A `data_dir` the process cannot write (a full disk, a read-only mount, a
/// wrong owner) must fail the attempt with a message naming the path — not
/// panic, not hang, and not leave the run without a terminal event.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_3_1_an_unwritable_data_dir_fails_cleanly_and_names_the_path() {
    kevin_testkit::skip_unless_pg!();
    let Some(readonly) = unwritable_dir() else {
        // Running as root (or on a filesystem that ignores the mode) makes the
        // injection impossible; skipping beats asserting something untrue.
        eprintln!("skipped: this user can write to a 0o555 directory");
        return;
    };
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("write somewhere impossible"))
            .with_plan(plan_of(1)),
    );
    // The fake worker creates its transcript directory under `data_dir`; under
    // a read-only parent that is `WorkerError::Io`.
    let worker: Arc<dyn Worker> = Arc::new(FakeWorker::new(
        Scenario::replying("done"),
        readonly.path().join("nope/transcripts"),
    ));
    let harness = Harness::boot(Setup::new().roles(roles).worker(worker)).await;
    let run = harness
        .start("write somewhere impossible", RunMode::Headless)
        .await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);
    let failures = payloads(&events, "task.attempt_failed");
    assert!(
        !failures.is_empty(),
        "no attempt failure recorded: {seen:?}"
    );
    let message = failures[0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("worker spawn failed"),
        "the failure must say the worker never started: {message}"
    );
    assert!(
        message.contains(&readonly.path().display().to_string()),
        "the failure must name the unwritable path: {message}"
    );
    // The run is terminal and the stream is intact: every started attempt has
    // an outcome.
    assert_eq!(
        count(&events, "task.attempt_started"),
        count(&events, "task.attempt_failed") + count(&events, "task.attempt_succeeded"),
        "an attempt was left in flight: {seen:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.envelope.event_type == "run.failed"
                || e.envelope.event_type == "run.completed"),
        "the run never terminalised: {seen:?}"
    );
    harness.shutdown().await;
}

/// A `0o555` temp directory, or `None` when the current user can write anyway.
fn unwritable_dir() -> Option<tempfile::TempDir> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().ok()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).ok()?;
        if std::fs::create_dir(dir.path().join("probe")).is_ok() {
            let _ = std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755));
            return None;
        }
        Some(dir)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

// ---------------------------------------------------------------------------
// (4) The worker binary disappears
// ---------------------------------------------------------------------------

/// A worker adapter whose binary is gone mid-run fails the attempt
/// **permanently**: retrying cannot bring the binary back, and burning the
/// attempt budget buries the real cause under "max attempts exhausted".
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_4_1_a_vanished_worker_binary_fails_permanently_without_a_retry_storm() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("run a binary that is gone"))
            .with_plan(plan_of(1)),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let inner: Arc<dyn Worker> = Arc::new(FakeWorker::new(Scenario::replying("done"), dir.path()));
    let vanishing = Arc::new(VanishingBinary::new(inner));
    let harness = Harness::boot(
        Setup::new()
            .roles(roles)
            .worker(Arc::clone(&vanishing) as Arc<dyn Worker>),
    )
    .await;
    let run = harness
        .start("run a binary that is gone", RunMode::Headless)
        .await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);
    let failures = payloads(&events, "task.attempt_failed");
    assert_eq!(failures.len(), 1, "exactly one attempt, no storm: {seen:?}");
    assert_eq!(
        failures[0]["class"], "permanent",
        "a missing binary is not transient: {seen:?}"
    );
    let message = failures[0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("binary not found"),
        "the operator must be told the binary is missing: {message}"
    );
    assert_eq!(count(&events, "task.retried"), 0, "{seen:?}");
    assert_eq!(
        vanishing.calls(),
        1,
        "the adapter was asked to start more than once"
    );
    assert_eq!(count(&events, "run.failed"), 1, "{seen:?}");
    harness.shutdown().await;
}

/// A worker whose binary has been deleted: every `start` reports
/// [`WorkerError::BinaryMissing`], exactly as the real supervisor does when
/// `spawn` returns `ErrorKind::NotFound`.
struct VanishingBinary {
    inner: Arc<dyn Worker>,
    calls: AtomicUsize,
}

impl std::fmt::Debug for VanishingBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VanishingBinary").finish_non_exhaustive()
    }
}

impl VanishingBinary {
    fn new(inner: Arc<dyn Worker>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Worker for VanishingBinary {
    fn kind(&self) -> WorkerKind {
        self.inner.kind()
    }

    async fn doctor(&self) -> kevin_worker::Doctor {
        self.inner.doctor().await
    }

    fn validate_alias(
        &self,
        alias: &ModelAlias,
        entry: &kevin_config::ModelEntry,
    ) -> Result<(), kevin_config::ConfigError> {
        self.inner.validate_alias(alias, entry)
    }

    async fn start(
        &self,
        _req: kevin_worker::TaskAttemptRequest,
    ) -> Result<kevin_worker::WorkerHandle, WorkerError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(WorkerError::BinaryMissing {
            kind: self.inner.kind(),
            bin: "/opt/kevin/bin/deleted-while-running".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// (5) A subscriber too slow for the bus
// ---------------------------------------------------------------------------

/// A projection that falls behind a bounded broadcast channel is told it lagged
/// and heals from the store; it must never silently skip the events it missed.
///
/// The bus half is `ac_ws04_2`; this is the consumer half — the one that
/// decides whether a read model is *wrong* after a burst.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_5_1_a_slow_projection_lags_and_still_reaches_the_head() {
    kevin_testkit::skip_unless_pg!();
    /// Far wider than the channel, so lag is certain rather than likely.
    const BURST: u32 = 200;
    let db = TestDb::new().await;
    let store = Arc::new(PgEventStore::new(db.pool().clone()));
    // Capacity 1 and no history: any burst larger than one event makes the
    // subscriber lag, which is the condition under test.
    let bus = Arc::new(InProcBus::new(InProcBusConfig {
        capacity: 1,
        history: 0,
    }));

    let cancel = CancellationToken::new();
    let runner = ProjectionRunner::new(
        by_name("task_board").expect("task_board projection"),
        db.pool().clone(),
        Arc::clone(&store) as Arc<dyn EventStore>,
    );
    let projection =
        tokio::spawn(runner.run(Arc::clone(&bus) as Arc<dyn EventBus>, cancel.clone()));

    // A burst far wider than the channel: the subscriber cannot keep up.
    let run_id = kevin_domain::RunId::new();
    let mut published = Vec::new();
    for i in 0..BURST {
        let task_id = kevin_domain::TaskId::new();
        let stream = kevin_store::StreamId::new("task", task_id.as_uuid());
        // Built through the domain type, so the payload is exactly what the
        // projection deserialises in production.
        let created = kevin_domain::task::TaskEvent::Created {
            task_id,
            run_id,
            kind: kevin_domain::TaskKind::Implement,
            spec: kevin_domain::TaskSpec::new(format!("t{i}"), "work"),
            budget: kevin_domain::Budget::unlimited(),
        };
        let appended = store
            .append(
                &stream,
                0,
                &[kevin_store::NewEvent {
                    event_id: kevin_domain::EventId::new(),
                    event_type: "task.created",
                    schema_version: 1,
                    occurred_at: chrono::Utc::now(),
                    correlation_id: run_id.as_uuid(),
                    causation_id: None,
                    actor: kevin_domain::Actor::system("ws25"),
                    payload: serde_json::to_value(&created).expect("payload"),
                }],
            )
            .await
            .expect("append");
        published.extend(appended.events.iter().map(|e| e.envelope.clone()));
    }
    // Publish in one go so the slow subscriber is guaranteed to overflow.
    bus.publish(&published).await.expect("publish");

    let head = store.head_position().await.expect("head");
    // The checkpoint is the observable: the projection is only correct once it
    // has *applied* everything up to the head, lag or no lag.
    common::eventually("the projection to reach the store head", || {
        futures_lite_block(async {
            let checkpoints = kevin_store::Checkpoints::new(db.pool().clone());
            checkpoints
                .get("task_board")
                .await
                .ok()
                .flatten()
                .unwrap_or(0)
                >= head
        })
    })
    .await;

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM orch.task_board")
        .fetch_one(db.pool())
        .await
        .expect("count task_board");
    assert_eq!(
        rows,
        i64::from(BURST),
        "the projection skipped the events it lagged on"
    );

    cancel.cancel();
    let _ = projection.await;
    db.close().await;
}

/// Runs `fut` to completion on the current thread. `eventually` takes a
/// synchronous predicate, and the checkpoint read is async.
fn futures_lite_block<F: Future<Output = bool>>(fut: F) -> bool {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

// ---------------------------------------------------------------------------
// (8) Postgres goes away under a live run
// ---------------------------------------------------------------------------

/// The database is restarted while a run is executing. The run must either
/// finish or fail with a message — what it may never do is lose events: every
/// attempt that started must have an outcome, and the run must terminalise.
#[tokio::test(flavor = "multi_thread")]
async fn ac_ws25_8_1_a_postgres_outage_mid_run_never_silently_loses_events() {
    kevin_testkit::skip_unless_pg!();
    let roles = Arc::new(
        ScriptedRoles::new()
            .with_understanding(understanding("survive an outage"))
            .with_plan(plan_of(4)),
    );
    let scenario = Scenario::replying("done").with_default(Rule::replying("done").delay_ms(150));
    let harness = Harness::boot(Setup::new().roles(roles).scenario(scenario)).await;
    let run = harness.start("survive an outage", RunMode::Headless).await;
    harness.wait_for(run, "task.attempt_started").await;

    // The outage: every backend of this database is terminated, so whichever
    // append is in flight loses its connection.
    let admin = sqlx::PgPool::connect(harness.db.admin_url())
        .await
        .expect("admin pool");
    let _ = sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(harness.db.name())
    .execute(&admin)
    .await;
    admin.close().await;

    let events = harness.wait_terminal(run).await;
    let seen = types(&events);

    // Whatever the outcome, the ledger is consistent.
    let started = count(&events, "task.attempt_started");
    let finished = count(&events, "task.attempt_failed") + count(&events, "task.attempt_succeeded");
    assert_eq!(
        started, finished,
        "an attempt has no outcome after the outage: {seen:?}"
    );
    assert_eq!(
        count(&events, "run.completed") + count(&events, "run.failed"),
        1,
        "the run neither completed nor failed exactly once: {seen:?}"
    );

    // Positions are strictly increasing and versions per stream are
    // contiguous: nothing was written twice and nothing was written half-way.
    let positions: Vec<u64> = events.iter().map(|e| e.position).collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "positions are not strictly increasing: {positions:?}"
    );
    let mut versions: std::collections::BTreeMap<uuid::Uuid, Vec<u64>> =
        std::collections::BTreeMap::new();
    for event in &events {
        versions
            .entry(event.envelope.aggregate_id)
            .or_default()
            .push(event.envelope.aggregate_version);
    }
    for (aggregate, mut seq) in versions {
        seq.sort_unstable();
        let expected: Vec<u64> = (1..=seq.len() as u64).collect();
        assert_eq!(seq, expected, "stream {aggregate} has a gap or a duplicate");
    }

    harness.shutdown().await;
}

/// Kept honest: `Duration` is used by the harness constants this file relies on.
const _: Duration = Duration::from_secs(0);
