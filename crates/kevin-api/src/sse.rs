//! Server-sent events (`plan/07-api-and-tui.md` §Event streams).
//!
//! Three endpoints share this machinery: `GET /api/v1/runs/{id}/events`,
//! `GET /api/v1/events` and `GET /api/v1/tasks/{id}/log/stream`.
//!
//! # The seam
//!
//! A client reconnects with `Last-Event-ID: <position>`. The handler
//!
//! 1. subscribes to the **live** bus first (so nothing produced during the
//!    catch-up is lost),
//! 2. replays `core.events` where `position > last` in batches,
//! 3. switches to the live subscription, dropping every message whose position
//!    is `<= ` the last position it already emitted — the *sequence guard* that
//!    makes the seam duplicate-free.
//!
//! Without `Last-Event-ID` the stream is live-only and opens with a synthetic
//! `snapshot` event carrying the current view, so a fresh client never has to
//! issue a second request to know where it stands.
//!
//! A [`kevin_bus::BusMessage::Lagged`] report means the broadcast channel
//! dropped events that can no longer be replayed on this subscription: the
//! stream emits `event: resync` and the client refetches a snapshot and
//! reconnects with the last position it has.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::Response;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use kevin_bus::{BusMessage, BusStream};
use serde::Serialize;
use uuid::Uuid;

use crate::dto::{EventDto, ResyncDto, SSE_RESYNC, SSE_SNAPSHOT};
use crate::port::EventsPort;
use crate::state::SsePermit;

/// Events read from the store per catch-up batch.
const CATCH_UP_BATCH: usize = 256;

/// Which events a subscriber wants.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    /// Keep only events correlated to this run.
    pub run_id: Option<Uuid>,
    /// Keep only these event types; entries may end in `*` to match a prefix
    /// (`run.*`). `None` keeps everything.
    pub types: Option<Vec<String>>,
}

impl EventFilter {
    /// Parses the `?types=run.*,task.attempt_started` query parameter.
    #[must_use]
    pub fn parse_types(types: Option<&str>) -> Option<Vec<String>> {
        let list: Vec<String> = types
            .into_iter()
            .flat_map(|raw| raw.split(','))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        (!list.is_empty()).then_some(list)
    }

    /// Whether `event` should be delivered.
    #[must_use]
    pub fn matches(&self, event: &EventDto) -> bool {
        if let Some(run_id) = self.run_id
            && event.correlation_id != run_id
        {
            return false;
        }
        match &self.types {
            None => true,
            Some(types) => types.iter().any(|pattern| {
                pattern.strip_suffix('*').map_or_else(
                    || pattern == &event.event_type,
                    |prefix| event.event_type.starts_with(prefix),
                )
            }),
        }
    }
}

/// One item of an event stream.
#[derive(Debug, Clone)]
pub enum Item {
    /// A domain event; `id:` is its global position.
    Event(Box<EventDto>),
    /// The bus dropped events; refetch a snapshot and reconnect.
    Resync(ResyncDto),
    /// The synthetic opening snapshot of a live-only connection.
    Snapshot(serde_json::Value),
}

impl Item {
    fn into_sse(self) -> SseEvent {
        match self {
            Item::Event(event) => SseEvent::default()
                .id(event.position.to_string())
                .event(event.event_type.clone())
                .json_data(&*event)
                .unwrap_or_else(|_| SseEvent::default().comment("unserialisable event")),
            Item::Resync(resync) => SseEvent::default()
                .event(SSE_RESYNC)
                .json_data(resync)
                .unwrap_or_else(|_| SseEvent::default().comment("resync")),
            Item::Snapshot(value) => SseEvent::default()
                .event(SSE_SNAPSHOT)
                .json_data(value)
                .unwrap_or_else(|_| SseEvent::default().comment("snapshot")),
        }
    }
}

/// Where a stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Live only; the stream opens with a `snapshot` event.
    Live,
    /// Replay everything after this position, then go live.
    After(u64),
}

impl Start {
    /// `Last-Event-ID` wins over `?from=`, per plan/07.
    #[must_use]
    pub fn resolve(last_event_id: Option<&str>, from: Option<u64>) -> Self {
        last_event_id
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .or(from)
            .map_or(Start::Live, Start::After)
    }
}

/// Phase of [`event_stream`].
#[derive(Debug)]
enum Phase {
    CatchUp { next: u64 },
    Live,
}

struct StreamState {
    events: Arc<dyn EventsPort>,
    live: BusStream,
    phase: Phase,
    queue: VecDeque<Item>,
    last_emitted: u64,
    filter: EventFilter,
    _permit: SsePermit,
}

/// Builds the catch-up + live stream behind the two event endpoints.
pub fn event_stream(
    events: Arc<dyn EventsPort>,
    start: Start,
    filter: EventFilter,
    snapshot: Option<serde_json::Value>,
    permit: SsePermit,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    // Subscribe *before* reading history so that events produced during the
    // catch-up are buffered by the bus rather than lost.
    let live = events.subscribe_live();
    let (phase, last_emitted) = match start {
        Start::Live => (Phase::Live, events.head()),
        Start::After(position) => (Phase::CatchUp { next: position }, position),
    };

    let mut queue = VecDeque::new();
    if let Some(snapshot) = snapshot {
        queue.push_back(Item::Snapshot(snapshot));
    }

    let state = StreamState {
        events,
        live,
        phase,
        queue,
        last_emitted,
        filter,
        _permit: permit,
    };

    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.queue.pop_front() {
                if let Item::Event(event) = &item {
                    state.last_emitted = state.last_emitted.max(event.position);
                }
                return Some((Ok(item.into_sse()), state));
            }

            match state.phase {
                Phase::CatchUp { next } => {
                    match state.events.after(next, CATCH_UP_BATCH).await {
                        Ok(batch) if batch.is_empty() => state.phase = Phase::Live,
                        Ok(batch) => {
                            let highest = batch.last().map_or(next, |e| e.position);
                            let short = batch.len() < CATCH_UP_BATCH;
                            state.queue.extend(
                                batch
                                    .into_iter()
                                    .filter(|event| state.filter.matches(event))
                                    .map(|event| Item::Event(Box::new(event))),
                            );
                            state.phase = if short {
                                Phase::Live
                            } else {
                                Phase::CatchUp { next: highest }
                            };
                            // Even when everything was filtered out, remember
                            // how far the catch-up got so the guard is right.
                            state.last_emitted = state.last_emitted.max(highest);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "SSE catch-up failed; going live");
                            state.phase = Phase::Live;
                        }
                    }
                }
                Phase::Live => match state.live.next().await {
                    None => return None,
                    Some(BusMessage::Lagged { from, to }) => {
                        tracing::warn!(
                            event = "kevin.bus.lagged",
                            from,
                            to,
                            "SSE subscriber lagged"
                        );
                        state.queue.push_back(Item::Resync(ResyncDto { from, to }));
                    }
                    Some(BusMessage::Live(bus_event)) => {
                        // Sequence guard across the catch-up/live seam.
                        if bus_event.position <= state.last_emitted {
                            continue;
                        }
                        let dto = crate::convert::bus_event(&bus_event);
                        if state.filter.matches(&dto) {
                            state.queue.push_back(Item::Event(Box::new(dto)));
                        } else {
                            state.last_emitted = state.last_emitted.max(bus_event.position);
                        }
                    }
                },
            }
        }
    })
}

/// Wraps a stream in an SSE response with the configured keep-alive.
pub fn respond<S>(stream: S, keepalive: Duration) -> Response
where
    S: Stream<Item = Result<SseEvent, Infallible>> + Send + 'static,
{
    use axum::response::IntoResponse;
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(keepalive).text("keepalive"))
        .into_response()
}

/// Serialises `value` as the `data:` of a one-off SSE event with an explicit id.
pub fn data_event(id: u64, name: &str, value: &impl Serialize) -> SseEvent {
    SseEvent::default()
        .id(id.to_string())
        .event(name)
        .json_data(value)
        .unwrap_or_else(|_| SseEvent::default().comment("unserialisable payload"))
}

#[cfg(test)]
mod tests {
    use super::{EventFilter, Start};
    use crate::dto::EventDto;

    fn event(event_type: &str, correlation: uuid::Uuid) -> EventDto {
        EventDto {
            position: 1,
            event_id: kevin_domain::ids::EventId::nil(),
            event_type: event_type.to_owned(),
            occurred_at: chrono::Utc::now(),
            aggregate_type: "run".to_owned(),
            aggregate_id: correlation,
            aggregate_version: 1,
            correlation_id: correlation,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn last_event_id_wins_over_the_from_parameter() {
        assert_eq!(Start::resolve(Some("42"), Some(7)), Start::After(42));
        assert_eq!(Start::resolve(None, Some(7)), Start::After(7));
        assert_eq!(Start::resolve(None, None), Start::Live);
        assert_eq!(Start::resolve(Some("not-a-number"), None), Start::Live);
    }

    #[test]
    fn type_patterns_match_prefixes_and_exact_names() {
        let run = uuid::Uuid::now_v7();
        let filter = EventFilter {
            run_id: None,
            types: EventFilter::parse_types(Some("run.*,task.attempt_started")),
        };
        assert!(filter.matches(&event("run.started", run)));
        assert!(filter.matches(&event("task.attempt_started", run)));
        assert!(!filter.matches(&event("task.created", run)));
        assert!(!filter.matches(&event("question.asked", run)));
    }

    #[test]
    fn the_run_filter_keeps_only_correlated_events() {
        let mine = uuid::Uuid::now_v7();
        let other = uuid::Uuid::now_v7();
        let filter = EventFilter {
            run_id: Some(mine),
            types: None,
        };
        assert!(filter.matches(&event("run.started", mine)));
        assert!(!filter.matches(&event("run.started", other)));
    }
}
