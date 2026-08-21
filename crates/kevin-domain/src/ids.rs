//! Identifier newtypes over uuid v7 (`plan/02-domain-model.md` §Identifiers).
//!
//! Every aggregate and every command/event carries one of these. v7 gives time
//! ordering; `Display`/`FromStr`/serde all use the plain hyphenated uuid string.
//! Production code obtains fresh ids through [`crate::IdGen`] so tests can
//! substitute a deterministic generator; `new()` is a convenience that uses the
//! system clock and RNG.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh uuid v7 id from the system clock.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing uuid (e.g. one loaded from the store).
            #[must_use]
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying uuid.
            #[must_use]
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// The all-zero id; useful as a placeholder in tests.
            #[must_use]
            pub const fn nil() -> Self {
                Self(Uuid::nil())
            }

            /// Whether this is the all-zero id.
            #[must_use]
            pub const fn is_nil(&self) -> bool {
                self.0.is_nil()
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Uuid {
                id.0
            }
        }

        impl AsRef<Uuid> for $name {
            fn as_ref(&self) -> &Uuid {
                &self.0
            }
        }
    };
}

define_id!(
    /// Identifier of a `Run` aggregate (also the `correlation_id` of every event it causes).
    RunId
);
define_id!(
    /// Identifier of a `Task` aggregate.
    TaskId
);
define_id!(
    /// Identifier of a `Question` aggregate.
    QuestionId
);
define_id!(
    /// Identifier of one worker attempt on a task.
    AttemptId
);
define_id!(
    /// Identifier of an `Evaluation` aggregate.
    EvaluationId
);
define_id!(
    /// Identifier of a `MemoryItem` aggregate.
    MemoryItemId
);
define_id!(
    /// Identifier of a persisted domain event (`EventEnvelope::event_id`).
    EventId
);
define_id!(
    /// Identifier of a command; doubles as the idempotency key in `core.processed_commands`.
    CommandId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_v7_and_unique() {
        let a = RunId::new();
        let b = RunId::new();
        assert_ne!(a, b);
        assert_eq!(a.as_uuid().get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn consecutive_ids_are_time_ordered() {
        let ids: Vec<TaskId> = (0..50).map(|_| TaskId::new()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn display_and_from_str_round_trip() {
        let id = QuestionId::new();
        let text = id.to_string();
        assert_eq!(text.len(), 36);
        assert_eq!(text.parse::<QuestionId>().unwrap(), id);
        assert!("not-a-uuid".parse::<QuestionId>().is_err());
    }

    #[test]
    fn serde_is_a_plain_uuid_string() {
        let id = EventId::from_uuid(Uuid::from_u128(0x0191_0000_0000_7000_8000_0000_0000_0001));
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"01910000-0000-7000-8000-000000000001\"");
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn converts_to_and_from_uuid() {
        let raw = Uuid::now_v7();
        let id = CommandId::from(raw);
        assert_eq!(Uuid::from(id), raw);
        assert_eq!(id.as_ref(), &raw);
        assert!(AttemptId::nil().is_nil());
        assert!(!MemoryItemId::new().is_nil());
        assert!(!EvaluationId::default().is_nil());
    }
}
