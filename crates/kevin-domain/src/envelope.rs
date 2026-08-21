//! Event envelope (`plan/02-domain-model.md` §Event envelope).
//!
//! Every persisted domain event is wrapped in an [`EventEnvelope`]. The shape
//! is frozen; `event_type`/`aggregate_type` are `&'static str` because they are
//! constants of the event catalog (`"run.started"`, `"run"`). Deserialising an
//! envelope (store catch-up, cross-process bus) interns those strings through
//! [`intern`], which leaks each *distinct* value once — bounded by the size of
//! the catalog.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::ids::EventId;
use crate::kinds::WorkerKind;

/// Who caused an event.
///
/// Serde form is internally tagged on `type` in `snake_case`, e.g.
/// `{"type":"user","name":"valentin"}`,
/// `{"type":"system","component":"orchestrator"}`,
/// `{"type":"worker","kind":"claude"}`,
/// `{"type":"kohral","agent_id":"…"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Actor {
    /// A human (CLI/TUI/API caller).
    User {
        /// User name as authenticated or given (`requested_by`).
        name: String,
    },
    /// Kevin itself (saga, scheduler, projections…).
    System {
        /// Component name, e.g. `orchestrator`, `evaluator`.
        component: String,
    },
    /// A worker adapter acting on behalf of a task attempt.
    Worker {
        /// Which worker.
        kind: WorkerKind,
    },
    /// The Kohral platform (Kohral-mode runs).
    Kohral {
        /// Kohral agent id.
        agent_id: String,
    },
}

impl Actor {
    /// Convenience constructor for [`Actor::User`].
    pub fn user(name: impl Into<String>) -> Self {
        Actor::User { name: name.into() }
    }

    /// Convenience constructor for [`Actor::System`].
    pub fn system(component: impl Into<String>) -> Self {
        Actor::System {
            component: component.into(),
        }
    }

    /// Convenience constructor for [`Actor::Worker`].
    #[must_use]
    pub const fn worker(kind: WorkerKind) -> Self {
        Actor::Worker { kind }
    }

    /// Convenience constructor for [`Actor::Kohral`].
    pub fn kohral(agent_id: impl Into<String>) -> Self {
        Actor::Kohral {
            agent_id: agent_id.into(),
        }
    }
}

/// Envelope around a domain event payload `E` (frozen shape, see module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EventEnvelope<E> {
    /// uuid v7 of the event itself.
    pub event_id: EventId,
    /// `"<context>.<past_tense>"`, e.g. `"run.started"`.
    pub event_type: &'static str,
    /// Per event type; bumped on breaking payload changes (additive evolution).
    pub schema_version: u16,
    /// When the event occurred (from the injected `Clock`).
    pub occurred_at: DateTime<Utc>,
    /// `"run"` | `"task"` | `"question"` | `"evaluation"` | `"route_score"` | `"memory_item"`.
    pub aggregate_type: &'static str,
    /// Id of the aggregate whose stream holds the event.
    pub aggregate_id: Uuid,
    /// 1-based position within the aggregate stream.
    pub aggregate_version: u64,
    /// Always the `RunId` when one exists.
    pub correlation_id: Uuid,
    /// `command_id` or `event_id` that caused this event.
    pub causation_id: Option<Uuid>,
    /// Who caused it.
    pub actor: Actor,
    /// The event itself (serialised as JSON by the store).
    pub payload: E,
}

impl<E> EventEnvelope<E> {
    /// Maps the payload, keeping every envelope field.
    pub fn map_payload<T>(self, f: impl FnOnce(E) -> T) -> EventEnvelope<T> {
        EventEnvelope {
            event_id: self.event_id,
            event_type: self.event_type,
            schema_version: self.schema_version,
            occurred_at: self.occurred_at,
            aggregate_type: self.aggregate_type,
            aggregate_id: self.aggregate_id,
            aggregate_version: self.aggregate_version,
            correlation_id: self.correlation_id,
            causation_id: self.causation_id,
            actor: self.actor,
            payload: f(self.payload),
        }
    }

    /// Fallible variant of [`Self::map_payload`].
    pub fn try_map_payload<T, Err>(
        self,
        f: impl FnOnce(E) -> Result<T, Err>,
    ) -> Result<EventEnvelope<T>, Err> {
        Ok(EventEnvelope {
            event_id: self.event_id,
            event_type: self.event_type,
            schema_version: self.schema_version,
            occurred_at: self.occurred_at,
            aggregate_type: self.aggregate_type,
            aggregate_id: self.aggregate_id,
            aggregate_version: self.aggregate_version,
            correlation_id: self.correlation_id,
            causation_id: self.causation_id,
            actor: self.actor,
            payload: f(self.payload)?,
        })
    }
}

/// Returns a `&'static str` equal to `s`, leaking `s` the first time a given
/// value is seen. Intended for catalog constants (event/aggregate types) read
/// back from storage; never call it with unbounded user input.
pub fn intern(s: &str) -> &'static str {
    static INTERNED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let set = INTERNED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = guard.get(s) {
        return existing;
    }
    let leaked: &'static str = Box::leak(s.to_owned().into_boxed_str());
    guard.insert(leaked);
    leaked
}

/// Wire shape used for deserialisation; `&'static str` fields are interned
/// through [`intern`] after reading (serde would otherwise demand `'de: 'static`).
#[derive(Deserialize)]
struct RawEnvelope<E> {
    event_id: EventId,
    event_type: String,
    schema_version: u16,
    occurred_at: DateTime<Utc>,
    aggregate_type: String,
    aggregate_id: Uuid,
    aggregate_version: u64,
    correlation_id: Uuid,
    causation_id: Option<Uuid>,
    actor: Actor,
    payload: E,
}

impl<'de, E: Deserialize<'de>> Deserialize<'de> for EventEnvelope<E> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawEnvelope::<E>::deserialize(deserializer)?;
        Ok(EventEnvelope {
            event_id: raw.event_id,
            event_type: intern(&raw.event_type),
            schema_version: raw.schema_version,
            occurred_at: raw.occurred_at,
            aggregate_type: intern(&raw.aggregate_type),
            aggregate_id: raw.aggregate_id,
            aggregate_version: raw.aggregate_version,
            correlation_id: raw.correlation_id,
            causation_id: raw.causation_id,
            actor: raw.actor,
            payload: raw.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;
    use crate::ids::RunId;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Started {
        goal: String,
    }

    fn sample() -> EventEnvelope<Started> {
        let run_id = RunId::from_uuid(Uuid::from_u128(0x0191_0000_0000_7000_8000_0000_0000_00aa));
        EventEnvelope {
            event_id: EventId::from_uuid(Uuid::from_u128(
                0x0191_0000_0000_7000_8000_0000_0000_00ee,
            )),
            event_type: "run.started",
            schema_version: 1,
            occurred_at: Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap(),
            aggregate_type: "run",
            aggregate_id: run_id.as_uuid(),
            aggregate_version: 1,
            correlation_id: run_id.as_uuid(),
            causation_id: Some(Uuid::from_u128(0x0191_0000_0000_7000_8000_0000_0000_00cc)),
            actor: Actor::user("valentin"),
            payload: Started {
                goal: "add /healthz".into(),
            },
        }
    }

    #[test]
    fn envelope_json_shape_is_frozen() {
        let value = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            value,
            json!({
                "event_id": "01910000-0000-7000-8000-0000000000ee",
                "event_type": "run.started",
                "schema_version": 1,
                "occurred_at": "2026-08-21T12:00:00Z",
                "aggregate_type": "run",
                "aggregate_id": "01910000-0000-7000-8000-0000000000aa",
                "aggregate_version": 1,
                "correlation_id": "01910000-0000-7000-8000-0000000000aa",
                "causation_id": "01910000-0000-7000-8000-0000000000cc",
                "actor": { "type": "user", "name": "valentin" },
                "payload": { "goal": "add /healthz" }
            })
        );
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let original = sample();
        let json = serde_json::to_string(&original).unwrap();
        let back: EventEnvelope<Started> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
        // Interned strings are pointer-stable across deserialisations.
        let again: EventEnvelope<Started> = serde_json::from_str(&json).unwrap();
        assert!(std::ptr::eq(back.event_type, again.event_type));
        assert_eq!(intern("run.started"), "run.started");
    }

    #[test]
    fn envelope_payload_can_be_erased_to_json_value() {
        let erased = sample().map_payload(|p| serde_json::to_value(p).unwrap());
        assert_eq!(erased.payload, json!({ "goal": "add /healthz" }));
        assert_eq!(erased.event_type, "run.started");
        let typed: EventEnvelope<Started> = erased.try_map_payload(serde_json::from_value).unwrap();
        assert_eq!(typed, sample());
    }

    #[test]
    fn actor_variants_serde() {
        let cases = [
            (Actor::user("v"), json!({"type":"user","name":"v"})),
            (
                Actor::system("orchestrator"),
                json!({"type":"system","component":"orchestrator"}),
            ),
            (
                Actor::worker(WorkerKind::Claude),
                json!({"type":"worker","kind":"claude"}),
            ),
            (
                Actor::kohral("agent-1"),
                json!({"type":"kohral","agent_id":"agent-1"}),
            ),
        ];
        for (actor, expected) in cases {
            assert_eq!(serde_json::to_value(&actor).unwrap(), expected);
            assert_eq!(serde_json::from_value::<Actor>(expected).unwrap(), actor);
        }
    }
}
