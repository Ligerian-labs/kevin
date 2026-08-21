//! `memory.*` events (`plan/02-domain-model.md` §Event catalog).
//!
//! The payloads are `kevin_domain::MemoryItemEvent` (WS-01); this module only
//! turns one of them into the [`NewEvent`] the store appends, so the memory
//! context never re-declares the catalog.

use chrono::Utc;
use kevin_domain::aggregate::EventMeta as _;
use kevin_domain::{Actor, EventId, MemoryItemEvent, MemoryItemId};
use kevin_store::event_store::{NewEvent, StreamId};

pub use kevin_domain::memory_item::MEMORY_ITEM_AGGREGATE_TYPE as AGGREGATE_TYPE;

/// `memory.item_stored`.
pub const ITEM_STORED: &str = "memory.item_stored";
/// `memory.item_superseded`.
pub const ITEM_SUPERSEDED: &str = "memory.item_superseded";
/// `memory.item_forgotten`.
pub const ITEM_FORGOTTEN: &str = "memory.item_forgotten";

/// The event stream of one memory item.
#[must_use]
pub fn stream(id: MemoryItemId) -> StreamId {
    StreamId::new(AGGREGATE_TYPE, id.as_uuid())
}

/// Wraps a domain event for the store, correlated on the item id.
pub fn new_event(
    id: MemoryItemId,
    event: &MemoryItemEvent,
    actor: Actor,
) -> Result<NewEvent, serde_json::Error> {
    Ok(NewEvent {
        event_id: EventId::new(),
        event_type: event.event_type(),
        schema_version: event.schema_version(),
        occurred_at: Utc::now(),
        correlation_id: id.as_uuid(),
        causation_id: None,
        actor,
        payload: serde_json::to_value(event)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_types_match_the_catalog() {
        assert_eq!(
            MemoryItemEvent::TYPES,
            [ITEM_STORED, ITEM_SUPERSEDED, ITEM_FORGOTTEN]
        );
        assert_eq!(AGGREGATE_TYPE, "memory_item");
    }

    #[test]
    fn a_wrapped_event_carries_the_catalog_metadata() {
        let id = MemoryItemId::new();
        let event = MemoryItemEvent::Forgotten {
            reason: "operator".to_owned(),
        };
        let wrapped = new_event(id, &event, Actor::system("memory")).unwrap();
        assert_eq!(wrapped.event_type, ITEM_FORGOTTEN);
        assert_eq!(wrapped.schema_version, 1);
        assert_eq!(wrapped.correlation_id, id.as_uuid());
        assert_eq!(wrapped.payload["type"], ITEM_FORGOTTEN);
        assert_eq!(stream(id).aggregate_type, AGGREGATE_TYPE);
    }
}
