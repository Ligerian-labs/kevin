//! [`BusStream`] and the shared subscription state machine used by both buses.
//!
//! A subscription owns a `broadcast::Receiver` of raw events plus a
//! [`CatchUp`] reader (store or in-memory history). It starts in catch-up mode
//! when resuming from a position, then goes live. Whenever the bounded
//! broadcast channel reports lag, the missed range is re-read from the
//! catch-up reader; only the part that cannot be replayed surfaces as
//! [`BusMessage::Lagged`]. Duplicates are suppressed by position, so the
//! handover from catch-up to live has neither gaps nor repeats.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::Stream;
use futures::stream::StreamExt as _;
use kevin_telemetry::{events, fields, metrics as metric_names};
use tokio::sync::broadcast;

use crate::{BusEvent, BusMessage, SourceError, SubscriptionFilter};

/// Default number of events read per catch-up round trip.
pub const DEFAULT_BATCH: usize = 256;

/// A batch read from a [`CatchUp`] reader.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    /// `Some(p)` when positions in `(after, p]` are no longer available
    /// (evicted history) — reported as [`BusMessage::Lagged`].
    pub evicted_to: Option<u64>,
    /// Events with `position > after`, ascending.
    pub events: Vec<Arc<BusEvent>>,
}

/// Position-addressed reader used for catch-up and lag healing.
#[async_trait]
pub(crate) trait CatchUp: Send + Sync {
    async fn read_after(&self, after: u64, limit: usize) -> Result<Batch, SourceError>;
}

/// Stream of [`BusMessage`]s. Implements `futures::Stream`; [`BusStream::next`]
/// is the ergonomic accessor.
pub struct BusStream {
    inner: Pin<Box<dyn Stream<Item = BusMessage> + Send>>,
}

impl std::fmt::Debug for BusStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusStream").finish_non_exhaustive()
    }
}

impl BusStream {
    /// Wraps any message stream.
    pub fn new(stream: impl Stream<Item = BusMessage> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }

    pub(crate) fn from_subscription(sub: Subscription) -> Self {
        Self::new(futures::stream::unfold(sub, |mut sub| async move {
            sub.next().await.map(|msg| (msg, sub))
        }))
    }

    /// Next message; `None` when the bus is closed.
    pub async fn next(&mut self) -> Option<BusMessage> {
        self.inner.next().await
    }

    /// Next live event, skipping lag reports; `None` when the bus is closed.
    pub async fn next_event(&mut self) -> Option<Arc<BusEvent>> {
        loop {
            match self.next().await? {
                BusMessage::Live(ev) => return Some(ev),
                BusMessage::Lagged { .. } => {}
            }
        }
    }
}

impl Stream for BusStream {
    type Item = BusMessage;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    CatchUp,
    Live,
}

pub(crate) struct Subscription {
    rx: broadcast::Receiver<Arc<BusEvent>>,
    catchup: Arc<dyn CatchUp>,
    filter: SubscriptionFilter,
    subscriber: String,
    batch: usize,
    /// Highest raw position consumed (delivered or filtered out).
    last: u64,
    mode: Mode,
    /// Set by a broadcast lag; healed before the next live event is delivered.
    heal: bool,
    pending: VecDeque<BusMessage>,
}

impl Subscription {
    /// `last` = position to resume after (`subscribe_from`) or the current
    /// position (`subscribe`); `catch_up` starts in catch-up mode.
    pub(crate) fn new(
        rx: broadcast::Receiver<Arc<BusEvent>>,
        catchup: Arc<dyn CatchUp>,
        filter: SubscriptionFilter,
        last: u64,
        catch_up: bool,
    ) -> Self {
        Self {
            rx,
            subscriber: filter.subscriber_label(),
            catchup,
            filter,
            batch: DEFAULT_BATCH,
            last,
            mode: if catch_up { Mode::CatchUp } else { Mode::Live },
            heal: false,
            pending: VecDeque::new(),
        }
    }

    async fn next(&mut self) -> Option<BusMessage> {
        loop {
            if let Some(msg) = self.pending.pop_front() {
                return Some(msg);
            }
            match self.mode {
                Mode::CatchUp => self.catch_up_round(None).await,
                Mode::Live => match self.rx.recv().await {
                    Ok(ev) => {
                        if self.heal {
                            self.heal = false;
                            self.heal_until(ev.position).await;
                        }
                        self.accept(ev);
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        self.heal = true;
                        metrics::counter!(metric_names::BUS_LAGGED_TOTAL, "subscriber" => self.subscriber.clone())
                            .increment(1);
                        tracing::warn!(
                            { fields::EVENT } = events::bus::LAGGED,
                            subscriber = %self.subscriber,
                            skipped,
                            after_position = self.last,
                            "bus subscriber lagged; healing from the event source"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return None;
                    }
                },
            }
        }
    }

    /// Pushes `ev` if it is new and passes the filter.
    fn accept(&mut self, ev: Arc<BusEvent>) {
        if ev.position <= self.last {
            return; // duplicate of something already replayed
        }
        self.last = ev.position;
        if self.filter.matches(&ev.envelope) {
            self.pending.push_back(BusMessage::Live(ev));
        }
    }

    /// One catch-up read. `below`: stop before this position (lag healing);
    /// `None`: read until the source is exhausted, then go live.
    async fn catch_up_round(&mut self, below: Option<u64>) {
        match self.catchup.read_after(self.last, self.batch).await {
            Ok(batch) => {
                if let Some(evicted_to) = batch.evicted_to.filter(|&e| e > self.last) {
                    let to = below.map_or(evicted_to, |b| evicted_to.min(b - 1));
                    self.pending.push_back(BusMessage::Lagged {
                        from: self.last + 1,
                        to,
                    });
                    self.last = to;
                }
                let mut count = 0usize;
                for ev in batch.events {
                    if below.is_some_and(|b| ev.position >= b) {
                        return;
                    }
                    count += 1;
                    self.accept(ev);
                }
                if count == 0 && below.is_none() {
                    self.mode = Mode::Live;
                }
            }
            Err(err) => {
                tracing::error!(
                    subscriber = %self.subscriber,
                    error = %err,
                    after_position = self.last,
                    "bus catch-up read failed"
                );
                if let Some(b) = below.filter(|&b| b > self.last + 1) {
                    self.pending.push_back(BusMessage::Lagged {
                        from: self.last + 1,
                        to: b - 1,
                    });
                    self.last = b - 1;
                }
                self.mode = Mode::Live;
            }
        }
    }

    /// Replays `(last, until)` after a broadcast lag.
    async fn heal_until(&mut self, until: u64) {
        while self.last + 1 < until {
            let before = self.last;
            self.catch_up_round(Some(until)).await;
            if self.last == before {
                // Source exhausted below `until` (store gap) — nothing more to replay.
                break;
            }
        }
    }
}

/// Catch-up reader over an [`crate::EventSource`] (never evicts).
pub(crate) struct SourceCatchUp(pub Arc<dyn crate::EventSource>);

#[async_trait]
impl CatchUp for SourceCatchUp {
    async fn read_after(&self, after: u64, limit: usize) -> Result<Batch, SourceError> {
        let events = self.0.read_all(after, limit).await?;
        Ok(Batch {
            evicted_to: None,
            events: events.into_iter().map(Arc::new).collect(),
        })
    }
}
