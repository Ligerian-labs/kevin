//! Transactional outbox relay (`core.outbox`).
//!
//! [`PgEventStore::append`](crate::PgEventStore) writes one outbox row per
//! event in the same transaction as the event. The relay reads undelivered
//! rows in position order, hands each batch to a handler (the in-process bus,
//! a cross-process notifier, …) and stamps `delivered_at` — in one
//! transaction with `SELECT … FOR UPDATE SKIP LOCKED`, so several relays can
//! run without delivering the same row twice concurrently.
//!
//! Guarantees: **at-least-once**. A crash between the handler's success and
//! the commit of `delivered_at` redelivers the batch on the next pass; a crash
//! between the event's commit and the relay (the "kill between commit and
//! relay" case) delivers it on the next pass — never zero times, and once the
//! row is stamped never again. Handlers must therefore be idempotent on
//! `position`. With a single relay, batches are delivered in position order.

use std::future::Future;
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::StoreError;
use crate::event_store::{EventRow, StoredEvent, to_i64};
use crate::upcast::Upcasters;

/// Error returned by an outbox handler; the batch is retried on the next pass.
#[derive(Debug, thiserror::Error)]
#[error("outbox delivery failed: {0}")]
pub struct DeliveryError(pub String);

impl DeliveryError {
    /// Builds a delivery error from any message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Summary of a relay pass or loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelayReport {
    /// Rows delivered and stamped.
    pub delivered: usize,
    /// Position of the last delivered row (`0` if none).
    pub last_position: u64,
}

/// The outbox relay.
#[derive(Debug, Clone)]
pub struct Outbox {
    pool: PgPool,
    upcasters: std::sync::Arc<Upcasters>,
    batch_size: usize,
    poll_interval: Duration,
}

impl Outbox {
    /// Relay over `pool` without upcasters, batch size 256, 1 s fallback poll.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            upcasters: std::sync::Arc::new(Upcasters::new()),
            batch_size: 256,
            poll_interval: Duration::from_secs(1),
        }
    }

    /// Applies `upcasters` to delivered events (use the store's registry).
    #[must_use]
    pub fn with_upcasters(mut self, upcasters: Upcasters) -> Self {
        self.upcasters = std::sync::Arc::new(upcasters);
        self
    }

    /// Maximum rows per handler call (≥ 1).
    #[must_use]
    pub fn batch_size(mut self, n: usize) -> Self {
        self.batch_size = n.max(1);
        self
    }

    /// How often [`Self::relay`] polls when no wake-up arrives (events appended
    /// by other processes do not bump the local position watch).
    #[must_use]
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Number of undelivered rows.
    pub async fn pending_count(&self) -> Result<u64, StoreError> {
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM core.outbox WHERE delivered_at IS NULL")
                .fetch_one(&self.pool)
                .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Delivers **one batch** of pending rows to `handler` and stamps them.
    /// Returns how many rows were delivered (`0` when nothing was pending).
    /// A handler error leaves the rows undelivered (attempt counter bumped)
    /// and is returned as `Err(DeliveryError)` inside `Ok`.
    pub async fn relay_once<F, Fut>(
        &self,
        handler: F,
    ) -> Result<Result<RelayReport, DeliveryError>, StoreError>
    where
        F: FnOnce(Vec<StoredEvent>) -> Fut,
        Fut: Future<Output = Result<(), DeliveryError>>,
    {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<EventRow> = sqlx::query_as(
            "SELECT e.position, e.event_id, e.event_type, e.schema_version, e.occurred_at, \
             e.aggregate_type, e.aggregate_id, e.aggregate_version, e.correlation_id, \
             e.causation_id, e.actor, e.payload \
             FROM core.outbox o JOIN core.events e ON e.position = o.position \
             WHERE o.delivered_at IS NULL ORDER BY o.position \
             LIMIT $1 FOR UPDATE OF o SKIP LOCKED",
        )
        .bind(i64::try_from(self.batch_size).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.rollback().await?;
            return Ok(Ok(RelayReport::default()));
        }
        let batch = rows
            .into_iter()
            .map(|r| {
                let stored = r.into_stored()?;
                Ok(StoredEvent {
                    position: stored.position,
                    envelope: self.upcasters.apply(stored.envelope),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let positions: Vec<i64> = batch
            .iter()
            .map(|e| to_i64(e.position))
            .collect::<Result<_, _>>()?;
        let last_position = batch.last().map_or(0, |e| e.position);
        let delivered = batch.len();

        match handler(batch).await {
            Ok(()) => {
                sqlx::query(
                    "UPDATE core.outbox SET delivered_at = now(), attempts = attempts + 1, \
                     last_error = NULL WHERE position = ANY($1)",
                )
                .bind(&positions)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Ok(RelayReport {
                    delivered,
                    last_position,
                }))
            }
            Err(err) => {
                // Release the row locks, then record the failure outside the
                // rolled-back transaction so the attempt is visible to operators.
                tx.rollback().await?;
                sqlx::query(
                    "UPDATE core.outbox SET attempts = attempts + 1, last_error = $2 \
                     WHERE position = ANY($1) AND delivered_at IS NULL",
                )
                .bind(&positions)
                .bind(&err.0)
                .execute(&self.pool)
                .await?;
                Ok(Err(err))
            }
        }
    }

    /// Drains everything pending right now (repeats [`Self::relay_once`] until
    /// a pass delivers nothing). Stops at the first handler error.
    pub async fn drain<F, Fut>(
        &self,
        mut handler: F,
    ) -> Result<Result<RelayReport, DeliveryError>, StoreError>
    where
        F: FnMut(Vec<StoredEvent>) -> Fut,
        Fut: Future<Output = Result<(), DeliveryError>>,
    {
        let mut total = RelayReport::default();
        loop {
            match self.relay_once(&mut handler).await? {
                Ok(report) if report.delivered == 0 => return Ok(Ok(total)),
                Ok(report) => {
                    total.delivered += report.delivered;
                    total.last_position = report.last_position;
                }
                Err(err) => return Ok(Err(err)),
            }
        }
    }

    /// Runs the relay until `cancel` fires: drains on start, then on every
    /// change of `wake` (the store's `subscribe_positions()`), and at least
    /// every `poll_interval`. Handler errors are logged and retried with the
    /// poll interval as back-off; database errors end the loop.
    pub async fn relay<F, Fut>(
        &self,
        mut handler: F,
        mut wake: watch::Receiver<u64>,
        cancel: CancellationToken,
    ) -> Result<RelayReport, StoreError>
    where
        F: FnMut(Vec<StoredEvent>) -> Fut,
        Fut: Future<Output = Result<(), DeliveryError>>,
    {
        let mut total = RelayReport::default();
        loop {
            match self.drain(&mut handler).await? {
                Ok(report) => {
                    total.delivered += report.delivered;
                    if report.delivered > 0 {
                        total.last_position = report.last_position;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "outbox delivery failed; retrying after poll interval");
                    tokio::select! {
                        () = cancel.cancelled() => return Ok(total),
                        () = tokio::time::sleep(self.poll_interval) => {}
                    }
                    continue;
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return Ok(total),
                changed = wake.changed() => {
                    if changed.is_err() {
                        // Sender dropped: fall back to polling only.
                        tokio::select! {
                            () = cancel.cancelled() => return Ok(total),
                            () = tokio::time::sleep(self.poll_interval) => {}
                        }
                    }
                }
                () = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    /// Deletes delivered rows stamped before `older_than` (for `kevin db prune`).
    pub async fn prune_delivered(&self, older_than: Duration) -> Result<u64, StoreError> {
        let secs = f64::from(u32::try_from(older_than.as_secs()).unwrap_or(u32::MAX));
        let done = sqlx::query(
            "DELETE FROM core.outbox WHERE delivered_at IS NOT NULL \
             AND delivered_at < now() - make_interval(secs => $1)",
        )
        .bind(secs)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
    }
}
