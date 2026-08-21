//! [`ProjectionRunner`] — drives one [`Projection`] (`plan/01-architecture.md`
//! §Read path, `plan/10-observability-ops.md` §Startup/Shutdown).
//!
//! Lifecycle: read the checkpoint (`core.projection_checkpoints`) → catch up
//! from the event store in batches → follow the bus. Each batch is applied in
//! **one transaction together with its checkpoint update**, so the read model
//! and the checkpoint can never disagree; a crash mid-batch just replays it,
//! which the projections' idempotent upserts absorb.
//!
//! A [`BusMessage::Lagged`] report is not a failure: the runner simply goes
//! back to the store and catches up from its checkpoint.

use std::sync::Arc;
use std::time::Instant;

use kevin_bus::{BusEvent, BusMessage, EventBus, SubscriptionFilter};
use kevin_store::{Checkpoints, EventStore, PgPool};
use kevin_telemetry::{events, fields, metrics as metric_names};
use tokio_util::sync::CancellationToken;

use super::{Projection, Result};

/// Events read per catch-up round trip.
pub const DEFAULT_BATCH: usize = 256;

/// What a [`ProjectionRunner::rebuild`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    /// Projection that was rebuilt.
    pub name: &'static str,
    /// Events replayed.
    pub events: u64,
    /// Checkpoint after the rebuild.
    pub position: u64,
}

/// Runs one projection against the store and the bus.
pub struct ProjectionRunner {
    projection: Box<dyn Projection>,
    pool: PgPool,
    store: Arc<dyn EventStore>,
    checkpoints: Checkpoints,
    batch: usize,
    last: u64,
}

impl std::fmt::Debug for ProjectionRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionRunner")
            .field("projection", &self.projection.name())
            .field("position", &self.last)
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

impl ProjectionRunner {
    /// A runner for `projection` reading from `store` and writing to `pool`.
    #[must_use]
    pub fn new(projection: Box<dyn Projection>, pool: PgPool, store: Arc<dyn EventStore>) -> Self {
        Self {
            projection,
            checkpoints: Checkpoints::new(pool.clone()),
            pool,
            store,
            batch: DEFAULT_BATCH,
            last: 0,
        }
    }

    /// Overrides the catch-up batch size.
    #[must_use]
    pub const fn with_batch(mut self, batch: usize) -> Self {
        self.batch = if batch == 0 { DEFAULT_BATCH } else { batch };
        self
    }

    /// The projection's name (checkpoint key).
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.projection.name()
    }

    /// Position this runner has processed up to.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.last
    }

    /// Loads the stored checkpoint into the runner.
    pub async fn load_checkpoint(&mut self) -> Result<u64> {
        self.last = self
            .checkpoints
            .get(self.projection.name())
            .await?
            .unwrap_or(0);
        Ok(self.last)
    }

    /// Applies everything the store has beyond the checkpoint and returns how
    /// many events were applied. Used on boot, on lag, and by `rebuild`.
    pub async fn catch_up(&mut self) -> Result<u64> {
        let mut applied = 0;
        loop {
            let events = self.store.read_all(self.last, self.batch).await?;
            if events.is_empty() {
                break;
            }
            let batch: Vec<BusEvent> = events
                .into_iter()
                .map(|e| BusEvent::new(e.position, e.envelope))
                .collect();
            applied += self.apply(&batch).await?;
        }
        self.record_lag().await?;
        Ok(applied)
    }

    /// Applies `events` (in position order) and the checkpoint in one
    /// transaction. Returns how many events the projection handled.
    pub async fn apply(&mut self, events: &[BusEvent]) -> Result<u64> {
        let Some(highest) = events.iter().map(|e| e.position).max() else {
            return Ok(0);
        };
        let started = Instant::now();
        let mut tx = self.pool.begin().await?;
        let mut handled = 0;
        for event in events {
            if event.position <= self.last {
                continue; // already applied before the crash / duplicate from the bus
            }
            if self.projection.handles(event.envelope.event_type) {
                self.projection.handle(event, &mut tx).await?;
                handled += 1;
            }
        }
        set_checkpoint(&mut tx, self.projection.name(), highest).await?;
        tx.commit().await?;
        self.last = self.last.max(highest);
        metrics::histogram!(
            metric_names::PROJECTION_APPLY_DURATION_SECONDS,
            "projection" => self.projection.name(),
        )
        .record(started.elapsed().as_secs_f64());
        tracing::debug!(
            { fields::EVENT } = events::store::PROJECTION_CHECKPOINT,
            projection = self.projection.name(),
            position = self.last,
            handled,
            "projection checkpoint advanced"
        );
        Ok(handled)
    }

    /// Empties the read model, forgets the checkpoint and replays every event
    /// from position 0.
    pub async fn rebuild(&mut self) -> Result<RebuildReport> {
        self.projection.reset(&self.pool).await?;
        self.checkpoints.delete(self.projection.name()).await?;
        self.last = 0;
        let events = self.catch_up().await?;
        tracing::info!(
            { fields::EVENT } = events::store::PROJECTION_REBUILT,
            projection = self.projection.name(),
            events,
            position = self.last,
            "projection rebuilt"
        );
        Ok(RebuildReport {
            name: self.projection.name(),
            events,
            position: self.last,
        })
    }

    /// Runs until `cancel` is triggered: catch up from the checkpoint, then
    /// follow `bus`, healing lag from the store.
    pub async fn run(mut self, bus: Arc<dyn EventBus>, cancel: CancellationToken) -> Result<()> {
        self.load_checkpoint().await?;
        let mut stream = bus.subscribe_from(
            self.last,
            SubscriptionFilter::all().named(format!("projection:{}", self.projection.name())),
        );
        self.catch_up().await?;
        loop {
            let message = tokio::select! {
                () = cancel.cancelled() => break,
                message = stream.next() => message,
            };
            let Some(message) = message else { break };
            match message {
                BusMessage::Live(event) => {
                    if event.position <= self.last {
                        continue;
                    }
                    if event.position == self.last + 1 {
                        self.apply(std::slice::from_ref(&event)).await?;
                    } else {
                        // A gap (filtered subscription, store gap, missed
                        // wake-up): the store is the source of truth.
                        self.catch_up().await?;
                    }
                    self.record_lag().await?;
                }
                BusMessage::Lagged { from, to } => {
                    tracing::warn!(
                        projection = self.projection.name(),
                        from,
                        to,
                        "projection missed bus events; catching up from the store"
                    );
                    self.catch_up().await?;
                }
            }
        }
        Ok(())
    }

    /// Publishes `kevin_projection_lag_events` for this projection.
    pub async fn record_lag(&self) -> Result<u64> {
        let head = head_position(&self.pool).await?;
        let lag = head.saturating_sub(self.last);
        #[allow(clippy::cast_precision_loss)]
        metrics::gauge!(
            metric_names::PROJECTION_LAG_EVENTS,
            "projection" => self.projection.name(),
        )
        .set(lag as f64);
        Ok(lag)
    }
}

/// Highest position in `core.events` (`0` when empty).
pub(crate) async fn head_position(pool: &PgPool) -> Result<u64> {
    let head: Option<i64> = sqlx::query_scalar("SELECT max(position) FROM core.events")
        .fetch_one(pool)
        .await?;
    Ok(head.unwrap_or(0).try_into().unwrap_or(0))
}

/// Checkpoint update inside the projection's own transaction.
async fn set_checkpoint(conn: &mut sqlx::PgConnection, name: &str, position: u64) -> Result<()> {
    sqlx::query(
        "INSERT INTO core.projection_checkpoints (name, position, updated_at) \
         VALUES ($1, $2, now()) \
         ON CONFLICT (name) DO UPDATE SET position = GREATEST(core.projection_checkpoints.position, EXCLUDED.position), updated_at = now()",
    )
    .bind(name)
    .bind(i64::try_from(position).unwrap_or(i64::MAX))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Rebuilds the projection called `name` (`kevin db rebuild-projection <name>`).
pub async fn rebuild(
    pool: PgPool,
    store: Arc<dyn EventStore>,
    name: &str,
) -> Result<RebuildReport> {
    let projection = super::by_name(name)?;
    ProjectionRunner::new(projection, pool, store)
        .rebuild()
        .await
}

/// Rebuilds every projection, in registry order
/// (`kevin db rebuild-projection --all`).
pub async fn rebuild_all(pool: PgPool, store: Arc<dyn EventStore>) -> Result<Vec<RebuildReport>> {
    let mut reports = Vec::new();
    for projection in super::all() {
        let mut runner = ProjectionRunner::new(projection, pool.clone(), Arc::clone(&store));
        reports.push(runner.rebuild().await?);
    }
    Ok(reports)
}

/// Spawns a runner per projection; every task stops on `cancel`.
///
/// Returns the join handles so the caller (the orchestrator's boot sequence)
/// can await a clean shutdown.
pub fn spawn_all(
    pool: &PgPool,
    store: &Arc<dyn EventStore>,
    bus: &Arc<dyn EventBus>,
    cancel: &CancellationToken,
) -> Vec<tokio::task::JoinHandle<Result<()>>> {
    super::all()
        .into_iter()
        .map(|projection| {
            let runner = ProjectionRunner::new(projection, pool.clone(), Arc::clone(store));
            let bus = Arc::clone(bus);
            let cancel = cancel.clone();
            tokio::spawn(async move { runner.run(bus, cancel).await })
        })
        .collect()
}
