//! Event bus platform crate (`plan/01-architecture.md` §Event-driven core).
//!
//! - [`EventBus`] — `publish`, `subscribe`, `subscribe_from`, `position`.
//! - [`InProcBus`] — `tokio::sync::broadcast` fan-out with a bounded in-memory
//!   history for catch-up and lag healing.
//! - [`PgNotifyBus`] — Postgres `LISTEN/NOTIFY` on `kevin_events` as a
//!   *wake-up*; every event is read back from an [`EventSource`] (the event
//!   store) by global position, so NOTIFY is never the source of truth.
//! - [`BusStream`] yields [`BusMessage::Live`] or [`BusMessage::Lagged`]:
//!   lag is reported, never silently dropped.
//!
//! Positions are the store's global `position` (1-based, increasing).
//! `from_position` arguments are **exclusive**: "resume after this position",
//! `0` meaning "from the beginning" — the same convention as projection
//! checkpoints and SSE `Last-Event-ID`.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`. `kevin-store` (WS-03) implements [`EventSource`].

pub mod filter;
pub mod inproc;
pub mod pg;
pub mod source;
pub mod stream;

use std::ops::Deref;
use std::sync::Arc;

use async_trait::async_trait;
use kevin_domain::EventEnvelope;

pub use filter::SubscriptionFilter;
pub use inproc::{InProcBus, InProcBusConfig};
pub use pg::{PgNotifyBus, PgNotifyBusConfig};
pub use source::{EventSource, SourceError};
pub use stream::BusStream;

/// A domain event with its payload erased to JSON (the form the store hands
/// back and the bus carries).
pub type Event = EventEnvelope<serde_json::Value>;

/// Name of the Postgres notification channel used as wake-up.
pub const PG_CHANNEL: &str = "kevin_events";

/// An event with its global store position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusEvent {
    /// Global position in the event store (1-based, increasing).
    pub position: u64,
    /// The envelope.
    pub envelope: Event,
}

impl BusEvent {
    /// Pairs an envelope with its position.
    #[must_use]
    pub fn new(position: u64, envelope: Event) -> Self {
        Self { position, envelope }
    }
}

impl Deref for BusEvent {
    type Target = Event;

    fn deref(&self) -> &Self::Target {
        &self.envelope
    }
}

/// One item of a [`BusStream`].
#[derive(Debug, Clone)]
pub enum BusMessage {
    /// An event, in position order.
    Live(Arc<BusEvent>),
    /// Events with positions in `from..=to` will **not** be delivered on this
    /// stream (the subscriber fell behind the bounded channel and the range is
    /// no longer replayable). Resync from the store if they matter.
    Lagged {
        /// First missed position.
        from: u64,
        /// Last missed position.
        to: u64,
    },
}

impl BusMessage {
    /// The event when this is [`BusMessage::Live`].
    #[must_use]
    pub fn live(&self) -> Option<&Arc<BusEvent>> {
        match self {
            BusMessage::Live(ev) => Some(ev),
            BusMessage::Lagged { .. } => None,
        }
    }

    /// Whether this is a lag report.
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        matches!(self, BusMessage::Lagged { .. })
    }
}

/// Errors from [`EventBus::publish`] and bus construction.
#[derive(Debug, thiserror::Error)]
pub enum BusError {
    /// Reading from the event source failed.
    #[error("event source: {0}")]
    Source(#[from] SourceError),
    /// Postgres error (listener, notify).
    #[error("postgres: {0}")]
    Pg(#[from] sqlx::Error),
    /// The bus has been shut down.
    #[error("bus closed")]
    Closed,
}

/// The event bus (frozen interface, plan/12 WS-04).
#[async_trait]
pub trait EventBus: Send + Sync {
    /// Fans out `events` to local subscribers (and, for cross-process buses,
    /// wakes other processes). Called **after** the events are committed.
    async fn publish(&self, events: &[Event]) -> Result<(), BusError>;

    /// Live subscription from the current position, filtered.
    fn subscribe(&self, filter: SubscriptionFilter) -> BusStream;

    /// Resume **after** `from_position`: replays `from_position+1..` from the
    /// source/history, then switches to live without gaps or duplicates.
    fn subscribe_from(&self, from_position: u64, filter: SubscriptionFilter) -> BusStream;

    /// Position of the last event this bus has fanned out (`0` = none yet).
    fn position(&self) -> u64;
}

#[async_trait]
impl<B: EventBus + ?Sized> EventBus for Arc<B> {
    async fn publish(&self, events: &[Event]) -> Result<(), BusError> {
        (**self).publish(events).await
    }

    fn subscribe(&self, filter: SubscriptionFilter) -> BusStream {
        (**self).subscribe(filter)
    }

    fn subscribe_from(&self, from_position: u64, filter: SubscriptionFilter) -> BusStream {
        (**self).subscribe_from(from_position, filter)
    }

    fn position(&self) -> u64 {
        (**self).position()
    }
}
