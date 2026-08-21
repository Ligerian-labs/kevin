//! [`PgNotifyBus`]: cross-process wake-ups over Postgres `LISTEN/NOTIFY`
//! (channel [`crate::PG_CHANNEL`]) with catch-up from an [`EventSource`].
//!
//! A pump task owns the listener. On every notification, local `publish`,
//! or poll tick it reads everything after its position from the source and
//! fans it out on a bounded broadcast channel; subscriptions (see
//! [`crate::stream`]) replay from the source when resuming or lagging. The
//! NOTIFY payload (the new head position) is only an optimisation hint.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use kevin_telemetry::{events, fields};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::stream::{CatchUp, SourceCatchUp, Subscription};
use crate::{
    BusError, BusEvent, BusStream, Event, EventBus, EventSource, PG_CHANNEL, SubscriptionFilter,
};

/// Tuning for [`PgNotifyBus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgNotifyBusConfig {
    /// Notification channel (`kevin_events`).
    pub channel: String,
    /// Broadcast channel capacity (per subscriber backlog before lag).
    pub capacity: usize,
    /// Events read per source round trip.
    pub batch: usize,
    /// Fallback poll interval (covers lost notifications / reconnects).
    pub poll_interval: Duration,
    /// Whether `publish` also issues `pg_notify` (the store's outbox relay
    /// does too; duplicates are harmless wake-ups).
    pub notify_on_publish: bool,
}

impl Default for PgNotifyBusConfig {
    fn default() -> Self {
        Self {
            channel: PG_CHANNEL.to_owned(),
            capacity: 1024,
            batch: 256,
            poll_interval: Duration::from_secs(1),
            notify_on_publish: true,
        }
    }
}

/// Cross-process event bus over Postgres. Clone to share; the pump stops
/// when the last clone is dropped.
#[derive(Debug, Clone)]
pub struct PgNotifyBus {
    inner: Arc<Inner>,
}

struct Inner {
    pool: PgPool,
    source: Arc<dyn EventSource>,
    cfg: PgNotifyBusConfig,
    tx: broadcast::Sender<Arc<BusEvent>>,
    position: AtomicU64,
    /// Position announced by the latest notification (hint only).
    announced: AtomicU64,
    wake: Arc<Notify>,
    drain_lock: Mutex<()>,
    cancel: CancellationToken,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgNotifyBus")
            .field("channel", &self.cfg.channel)
            .field("position", &self.position.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            task.abort();
        }
    }
}

impl PgNotifyBus {
    /// Starts listening on `kevin_events` with default tuning. Only events
    /// committed after construction are fanned out live; older ones are
    /// reachable through [`EventBus::subscribe_from`].
    pub async fn new(pool: PgPool, source: Arc<dyn EventSource>) -> Result<Self, BusError> {
        Self::with_config(pool, source, PgNotifyBusConfig::default()).await
    }

    /// [`Self::new`] with explicit tuning.
    pub async fn with_config(
        pool: PgPool,
        source: Arc<dyn EventSource>,
        cfg: PgNotifyBusConfig,
    ) -> Result<Self, BusError> {
        let head = source.latest_position().await?;
        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen(&cfg.channel).await?;
        let (tx, _rx) = broadcast::channel(cfg.capacity.max(1));
        let inner = Arc::new(Inner {
            pool,
            source,
            cfg,
            tx,
            position: AtomicU64::new(head),
            announced: AtomicU64::new(head),
            wake: Arc::new(Notify::new()),
            drain_lock: Mutex::new(()),
            cancel: CancellationToken::new(),
            tasks: std::sync::Mutex::new(Vec::new()),
        });
        let listen_task = tokio::spawn(listen_loop(listener, Arc::downgrade(&inner)));
        let pump_task = tokio::spawn(pump_loop(
            Arc::downgrade(&inner),
            Arc::clone(&inner.wake),
            inner.cancel.clone(),
            inner.cfg.poll_interval,
        ));
        inner
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend([listen_task, pump_task]);
        Ok(Self { inner })
    }

    /// Reads everything after the current position from the source and fans
    /// it out. Serialised, so concurrent callers never reorder or duplicate.
    pub async fn drain(&self) -> Result<u64, BusError> {
        self.inner.drain().await
    }

    /// Sends `pg_notify(channel, position)` so other processes wake up.
    pub async fn notify(&self, position: u64) -> Result<(), BusError> {
        self.inner.notify(position).await
    }
}

impl Inner {
    async fn drain(&self) -> Result<u64, BusError> {
        let _guard = self.drain_lock.lock().await;
        loop {
            let after = self.position.load(Ordering::Acquire);
            let batch = self.source.read_all(after, self.cfg.batch).await?;
            let n = batch.len();
            for ev in batch {
                self.position.store(ev.position, Ordering::Release);
                let _ = self.tx.send(Arc::new(ev));
            }
            if n < self.cfg.batch {
                return Ok(self.position.load(Ordering::Acquire));
            }
        }
    }

    async fn notify(&self, position: u64) -> Result<(), BusError> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(&self.cfg.channel)
            .bind(position.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

async fn listen_loop(mut listener: PgListener, inner: std::sync::Weak<Inner>) {
    loop {
        let Some(strong) = inner.upgrade() else {
            return;
        };
        if strong.cancel.is_cancelled() {
            return;
        }
        let cancel = strong.cancel.clone();
        drop(strong);
        let received = tokio::select! {
            () = cancel.cancelled() => return,
            r = listener.recv() => r,
        };
        let Some(strong) = inner.upgrade() else {
            return;
        };
        match received {
            Ok(notification) => {
                if let Ok(pos) = notification.payload().trim().parse::<u64>() {
                    strong.announced.fetch_max(pos, Ordering::AcqRel);
                }
                strong.wake.notify_one();
            }
            Err(err) => {
                tracing::warn!(error = %err, channel = %strong.cfg.channel, "pg listener error; reconnecting");
                // The listener reconnects on the next recv(); notifications sent
                // meanwhile are lost, so wake the pump to catch up.
                strong.wake.notify_one();
                drop(strong);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Waits for a wake-up (notification, local publish) or the poll tick, then
/// drains. Holds the bus only while draining, so dropping the last
/// [`PgNotifyBus`] clone stops the pump.
async fn pump_loop(
    inner: std::sync::Weak<Inner>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
    poll: Duration,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = wake.notified() => {}
            () = tokio::time::sleep(poll) => {}
        }
        let Some(strong) = inner.upgrade() else {
            return;
        };
        if let Err(err) = strong.drain().await {
            tracing::warn!(error = %err, "pg bus catch-up failed; will retry");
            drop(strong);
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

#[async_trait]
impl EventBus for PgNotifyBus {
    /// Wake-up: `events` are already committed in the store; this fans out
    /// everything new from the source to local subscribers, then notifies
    /// other processes with the head position.
    async fn publish(&self, events: &[Event]) -> Result<(), BusError> {
        if self.inner.cancel.is_cancelled() {
            return Err(BusError::Closed);
        }
        let head = self.inner.drain().await?;
        if !events.is_empty() {
            tracing::trace!(
                { fields::EVENT } = events::store::OUTBOX_RELAYED,
                count = events.len(),
                head,
                "published to pg bus"
            );
        }
        if self.inner.cfg.notify_on_publish {
            self.inner.notify(head).await?;
        }
        Ok(())
    }

    fn subscribe(&self, filter: SubscriptionFilter) -> BusStream {
        let last = self.inner.position.load(Ordering::Acquire);
        let rx = self.inner.tx.subscribe();
        let catchup: Arc<dyn CatchUp> = Arc::new(SourceCatchUp(Arc::clone(&self.inner.source)));
        BusStream::from_subscription(Subscription::new(rx, catchup, filter, last, false))
    }

    fn subscribe_from(&self, from_position: u64, filter: SubscriptionFilter) -> BusStream {
        let rx = self.inner.tx.subscribe();
        let catchup: Arc<dyn CatchUp> = Arc::new(SourceCatchUp(Arc::clone(&self.inner.source)));
        BusStream::from_subscription(Subscription::new(rx, catchup, filter, from_position, true))
    }

    fn position(&self) -> u64 {
        self.inner.position.load(Ordering::Acquire)
    }
}
