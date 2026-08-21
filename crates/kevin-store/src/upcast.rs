//! Upcaster registry (`plan/02-domain-model.md` §Event catalog).
//!
//! Stored events are never rewritten. When an event type's payload changes
//! shape, the producer bumps `schema_version` and registers an upcaster
//! `(event_type, from_version) -> fn(payload) -> payload` that lifts a
//! `from_version` payload to `from_version + 1`. [`Upcasters::apply`] chains
//! registered steps until no upcaster exists for the current version, so a
//! v1 event is read as v3 if `v1→v2` and `v2→v3` are registered. The store
//! applies the registry on every read (`load_stream`, `read_all`, outbox relay).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use kevin_domain::EventEnvelope;
use serde_json::Value;

/// A single upcasting step: lifts a payload from version `n` to `n + 1`.
pub type UpcastFn = dyn Fn(Value) -> Value + Send + Sync + 'static;

/// Registry of upcasting steps keyed by `(event_type, from_version)`.
#[derive(Clone, Default)]
pub struct Upcasters {
    steps: HashMap<(String, u16), Arc<UpcastFn>>,
}

impl fmt::Debug for Upcasters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut keys: Vec<_> = self.steps.keys().collect();
        keys.sort();
        f.debug_struct("Upcasters").field("steps", &keys).finish()
    }
}

impl Upcasters {
    /// An empty registry (events are returned as stored).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the step lifting `event_type` payloads from `from_version` to
    /// `from_version + 1`. Re-registering the same key replaces the step.
    pub fn register<F>(
        &mut self,
        event_type: impl Into<String>,
        from_version: u16,
        f: F,
    ) -> &mut Self
    where
        F: Fn(Value) -> Value + Send + Sync + 'static,
    {
        self.steps
            .insert((event_type.into(), from_version), Arc::new(f));
        self
    }

    /// Builder-style [`Self::register`].
    #[must_use]
    pub fn with<F>(mut self, event_type: impl Into<String>, from_version: u16, f: F) -> Self
    where
        F: Fn(Value) -> Value + Send + Sync + 'static,
    {
        self.register(event_type, from_version, f);
        self
    }

    /// Number of registered steps.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Whether no step is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The version `event_type` payloads are lifted to when starting from
    /// `from_version` (i.e. `from_version` when no step is registered).
    #[must_use]
    pub fn target_version(&self, event_type: &str, from_version: u16) -> u16 {
        let mut version = from_version;
        while self.steps.contains_key(&(event_type.to_owned(), version)) {
            version = version.saturating_add(1);
            if version == u16::MAX {
                break;
            }
        }
        version
    }

    /// Lifts `envelope.payload` through every registered step for its event
    /// type, updating `schema_version` accordingly.
    #[must_use]
    pub fn apply(&self, envelope: EventEnvelope<Value>) -> EventEnvelope<Value> {
        if self.steps.is_empty() {
            return envelope;
        }
        let mut envelope = envelope;
        let event_type = envelope.event_type.to_owned();
        loop {
            let key = (event_type.clone(), envelope.schema_version);
            let Some(step) = self.steps.get(&key) else {
                break;
            };
            let payload = std::mem::take(&mut envelope.payload);
            envelope.payload = step(payload);
            envelope.schema_version = envelope.schema_version.saturating_add(1);
            if envelope.schema_version == u16::MAX {
                break;
            }
        }
        envelope
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kevin_domain::{Actor, EventId};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn envelope(schema_version: u16, payload: Value) -> EventEnvelope<Value> {
        EventEnvelope {
            event_id: EventId::nil(),
            event_type: "run.started",
            schema_version,
            occurred_at: Utc::now(),
            aggregate_type: "run",
            aggregate_id: Uuid::nil(),
            aggregate_version: 1,
            correlation_id: Uuid::nil(),
            causation_id: None,
            actor: Actor::system("test"),
            payload,
        }
    }

    #[test]
    fn chains_consecutive_steps() {
        let ups = Upcasters::new()
            .with("run.started", 1, |mut p| {
                p["mode"] = json!("interactive");
                p
            })
            .with("run.started", 2, |mut p| {
                p["goal"] = json!({ "text": p["goal"].clone() });
                p
            });
        let out = ups.apply(envelope(1, json!({ "goal": "x" })));
        assert_eq!(out.schema_version, 3);
        assert_eq!(
            out.payload,
            json!({ "goal": { "text": "x" }, "mode": "interactive" })
        );
        assert_eq!(ups.target_version("run.started", 1), 3);
        assert_eq!(ups.target_version("run.started", 3), 3);
        assert_eq!(ups.target_version("task.created", 1), 1);
    }

    #[test]
    fn leaves_other_types_and_versions_alone() {
        let ups = Upcasters::new().with("run.started", 1, |_| json!("changed"));
        let same_type_newer = ups.apply(envelope(2, json!(1)));
        assert_eq!(same_type_newer.schema_version, 2);
        assert_eq!(same_type_newer.payload, json!(1));
        let mut other = envelope(1, json!(1));
        other.event_type = "task.created";
        let other = ups.apply(other);
        assert_eq!(other.payload, json!(1));
        assert_eq!(other.schema_version, 1);
    }

    #[test]
    fn empty_registry_is_identity() {
        let ups = Upcasters::new();
        assert!(ups.is_empty());
        let env = envelope(1, json!({ "a": 1 }));
        assert_eq!(ups.apply(env.clone()), env);
    }
}
