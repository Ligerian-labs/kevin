//! In-memory [`EventSource`] and envelope builders for bus tests (WS-04).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use kevin_bus::{BusEvent, Event, EventSource, SourceError};
use kevin_domain::{Actor, EventEnvelope, EventId};
use uuid::Uuid;

/// A `Vec`-backed event source standing in for the event store. Positions are
/// assigned on [`VecEventSource::push`] (1, 2, 3, …) unless
/// [`VecEventSource::push_at`] sets them explicitly (gaps allowed).
#[derive(Debug, Default, Clone)]
pub struct VecEventSource {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    events: Mutex<Vec<BusEvent>>,
    failing: AtomicBool,
}

impl VecEventSource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `envelope` at the next position and returns that position.
    pub fn push(&self, envelope: Event) -> u64 {
        let mut events = self.lock();
        let position = events.last().map_or(0, |e| e.position) + 1;
        events.push(BusEvent::new(position, envelope));
        position
    }

    /// Appends at an explicit position (must exceed the current head).
    pub fn push_at(&self, position: u64, envelope: Event) {
        let mut events = self.lock();
        let head = events.last().map_or(0, |e| e.position);
        assert!(
            position > head,
            "push_at({position}) must exceed head {head}"
        );
        events.push(BusEvent::new(position, envelope));
    }

    /// Appends several envelopes; returns the last position.
    pub fn push_all(&self, envelopes: impl IntoIterator<Item = Event>) -> u64 {
        let mut last = 0;
        for env in envelopes {
            last = self.push(env);
        }
        last
    }

    /// Makes every read fail (simulates the store being unreachable).
    pub fn set_failing(&self, failing: bool) {
        self.inner.failing.store(failing, Ordering::SeqCst);
    }

    /// Number of stored events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the source is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Highest position (`0` when empty).
    #[must_use]
    pub fn head(&self) -> u64 {
        self.lock().last().map_or(0, |e| e.position)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<BusEvent>> {
        self.inner
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl EventSource for VecEventSource {
    async fn read_all(
        &self,
        from_position: u64,
        limit: usize,
    ) -> Result<Vec<BusEvent>, SourceError> {
        if self.inner.failing.load(Ordering::SeqCst) {
            return Err(SourceError::msg("VecEventSource: simulated failure"));
        }
        Ok(self
            .lock()
            .iter()
            .filter(|e| e.position > from_position)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn latest_position(&self) -> Result<u64, SourceError> {
        if self.inner.failing.load(Ordering::SeqCst) {
            return Err(SourceError::msg("VecEventSource: simulated failure"));
        }
        Ok(self.head())
    }
}

/// A deterministic `run.*` envelope for `run_id` with aggregate version `n`
/// (`event_type` = `run.started` for `n == 1`, `run.progressed` otherwise).
#[must_use]
pub fn run_event(run_id: Uuid, n: u64) -> Event {
    event(
        run_id,
        "run",
        run_id,
        if n == 1 {
            "run.started"
        } else {
            "run.progressed"
        },
        n,
    )
}

/// A deterministic envelope with the given coordinates.
#[must_use]
pub fn event(
    run_id: Uuid,
    aggregate_type: &'static str,
    aggregate_id: Uuid,
    event_type: &'static str,
    version: u64,
) -> Event {
    EventEnvelope {
        event_id: EventId::new(),
        event_type,
        schema_version: 1,
        occurred_at: Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .unwrap_or_default()
            + chrono::TimeDelta::seconds(i64::try_from(version).unwrap_or(0)),
        aggregate_type,
        aggregate_id,
        aggregate_version: version,
        correlation_id: run_id,
        causation_id: None,
        actor: Actor::system("testkit"),
        payload: serde_json::json!({ "n": version }),
    }
}
