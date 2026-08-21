//! The [`Aggregate`] and [`EventMeta`] traits every event-sourced aggregate
//! implements (`plan/01-architecture.md` §Write path, `plan/12-workstreams.md`
//! WS-01 frozen interface).
//!
//! ```text
//! load stream → rehydrate → handle(command) → Vec<Event> → append → apply
//! ```
//!
//! `handle` never mutates; `apply` never fails. `rehydrate` is `Default` plus
//! `apply` for every stored event, so an aggregate's state is a pure function
//! of its stream.

use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::error::DomainError;

/// A persisted domain event: knows its catalog name and payload schema version.
pub trait EventMeta: Debug + Clone + Serialize + DeserializeOwned {
    /// Catalog name, `"<context>.<past_tense>"` (e.g. `run.started`).
    fn event_type(&self) -> &'static str;

    /// Payload schema version for this event type (starts at 1, bumped on
    /// breaking changes; the store upcasts older versions on load).
    fn schema_version(&self) -> u16;

    /// Aggregate type the event belongs to (`"run"`, `"task"`, …).
    fn aggregate_type(&self) -> &'static str;
}

/// An event-sourced aggregate.
pub trait Aggregate: Default + Debug {
    /// Commands the aggregate handles.
    type Command: Debug;
    /// Events the aggregate emits and applies.
    type Event: EventMeta;

    /// Aggregate type name used in envelopes and stream ids.
    const TYPE: &'static str;

    /// The aggregate id (nil before the creating event).
    fn id(&self) -> Uuid;

    /// Number of events applied so far (0 = does not exist yet).
    fn version(&self) -> u64;

    /// Decides: returns the events a command produces, or why it is rejected.
    /// Never mutates `self`.
    fn handle(&self, cmd: &Self::Command) -> Result<Vec<Self::Event>, DomainError>;

    /// Evolves state with one event and bumps `version`. Infallible.
    fn apply(&mut self, event: &Self::Event);

    /// Rebuilds an aggregate from its stream.
    fn rehydrate<'a, I>(events: I) -> Self
    where
        Self::Event: 'a,
        I: IntoIterator<Item = &'a Self::Event>,
    {
        let mut aggregate = Self::default();
        for event in events {
            aggregate.apply(event);
        }
        aggregate
    }

    /// `handle` followed by `apply` of every produced event; returns the events.
    /// Convenience for tests and in-memory drivers.
    fn execute(&mut self, cmd: &Self::Command) -> Result<Vec<Self::Event>, DomainError> {
        let events = self.handle(cmd)?;
        for event in &events {
            self.apply(event);
        }
        Ok(events)
    }

    /// `true` once the creating event has been applied.
    fn exists(&self) -> bool {
        self.version() > 0
    }
}
