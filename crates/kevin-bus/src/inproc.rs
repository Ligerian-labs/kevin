//! [`InProcBus`]: `tokio::sync::broadcast` fan-out for one process, with a
//! bounded in-memory history so subscribers can resume from a position and
//! lagging subscribers are healed instead of losing events.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::stream::{Batch, CatchUp, Subscription};
use crate::{BusError, BusEvent, BusStream, Event, EventBus, SubscriptionFilter};

/// Tuning for [`InProcBus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InProcBusConfig {
    /// Broadcast channel capacity (per subscriber backlog before lag).
    pub capacity: usize,
    /// Events retained for catch-up / lag healing (`0` = none: lag and
    /// resume-from-older-positions surface as `Lagged`).
    pub history: usize,
}

impl Default for InProcBusConfig {
    fn default() -> Self {
        Self {
            capacity: 1024,
            history: 4096,
        }
    }
}

/// In-process event bus. Positions are assigned by the bus itself
/// (1, 2, 3, …) at publish time.
#[derive(Debug, Clone)]
pub struct InProcBus {
    inner: Arc<Inner>,
}

/// The sender lives only in the bus, so dropping every clone closes the
/// channel (subscriptions end); the history is shared with subscriptions.
#[derive(Debug)]
struct Inner {
    tx: broadcast::Sender<Arc<BusEvent>>,
    history: Arc<History>,
}

/// Bounded event history + position counter, shared with subscriptions for
/// catch-up and lag healing.
#[derive(Debug)]
struct History {
    state: Mutex<State>,
    cap: usize,
}

#[derive(Debug, Default)]
struct State {
    last: u64,
    events: VecDeque<Arc<BusEvent>>,
}

impl Default for InProcBus {
    fn default() -> Self {
        Self::new(InProcBusConfig::default())
    }
}

impl InProcBus {
    /// A bus with the given capacity/history.
    #[must_use]
    pub fn new(cfg: InProcBusConfig) -> Self {
        let (tx, _rx) = broadcast::channel(cfg.capacity.max(1));
        Self {
            inner: Arc::new(Inner {
                tx,
                history: Arc::new(History {
                    state: Mutex::new(State::default()),
                    cap: cfg.history,
                }),
            }),
        }
    }

    /// A bus with default tuning.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.history.lock()
    }

    fn catchup(&self) -> Arc<dyn CatchUp> {
        Arc::clone(&self.inner.history) as Arc<dyn CatchUp>
    }
}

impl History {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl CatchUp for History {
    async fn read_after(&self, after: u64, limit: usize) -> Result<Batch, crate::SourceError> {
        let state = self.lock();
        let oldest = state.events.front().map_or(state.last + 1, |e| e.position);
        let evicted_to = (after + 1 < oldest).then_some(oldest - 1);
        let events = state
            .events
            .iter()
            .filter(|e| e.position > after)
            .take(limit)
            .cloned()
            .collect();
        Ok(Batch { evicted_to, events })
    }
}

#[async_trait]
impl EventBus for InProcBus {
    async fn publish(&self, events: &[Event]) -> Result<(), BusError> {
        let mut state = self.lock();
        for envelope in events {
            state.last += 1;
            let ev = Arc::new(BusEvent::new(state.last, envelope.clone()));
            let cap = self.inner.history.cap;
            if cap > 0 {
                if state.events.len() >= cap {
                    state.events.pop_front();
                }
                state.events.push_back(Arc::clone(&ev));
            }
            // No receivers is not an error: events stay in history.
            let _ = self.inner.tx.send(ev);
        }
        Ok(())
    }

    fn subscribe(&self, filter: SubscriptionFilter) -> BusStream {
        let (rx, last) = {
            let state = self.lock();
            (self.inner.tx.subscribe(), state.last)
        };
        BusStream::from_subscription(Subscription::new(rx, self.catchup(), filter, last, false))
    }

    fn subscribe_from(&self, from_position: u64, filter: SubscriptionFilter) -> BusStream {
        let rx = {
            let _state = self.lock();
            self.inner.tx.subscribe()
        };
        BusStream::from_subscription(Subscription::new(
            rx,
            self.catchup(),
            filter,
            from_position,
            true,
        ))
    }

    fn position(&self) -> u64 {
        self.lock().last
    }
}
