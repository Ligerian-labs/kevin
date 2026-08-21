//! Time and id generation abstractions (`plan/11-testing.md` §Determinism rules).
//!
//! Production wires [`SystemClock`] and [`UuidV7IdGen`]; tests use the fakes
//! from `kevin-testkit::clock` (`FixedClock`, `SeqIdGen`) so timestamps and ids
//! in snapshots are stable. Both traits are object-safe and meant to be shared
//! as `Arc<dyn Clock>` / `Arc<dyn IdGen>`.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::ids::{
    AttemptId, CommandId, EvaluationId, EventId, MemoryItemId, QuestionId, RunId, TaskId,
};

/// Source of the current time.
pub trait Clock: Send + Sync {
    /// The current instant in UTC.
    fn now(&self) -> DateTime<Utc>;
}

/// Wall-clock time from the operating system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

impl<C: Clock + ?Sized> Clock for Arc<C> {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now(&self) -> DateTime<Utc> {
        (**self).now()
    }
}

/// Source of fresh uuid v7 identifiers.
///
/// Only [`IdGen::next_id`] is required; the typed helpers wrap it.
pub trait IdGen: Send + Sync {
    /// A fresh uuid (v7 in production).
    fn next_id(&self) -> Uuid;

    /// Fresh [`RunId`].
    fn run_id(&self) -> RunId {
        RunId::from_uuid(self.next_id())
    }
    /// Fresh [`TaskId`].
    fn task_id(&self) -> TaskId {
        TaskId::from_uuid(self.next_id())
    }
    /// Fresh [`QuestionId`].
    fn question_id(&self) -> QuestionId {
        QuestionId::from_uuid(self.next_id())
    }
    /// Fresh [`AttemptId`].
    fn attempt_id(&self) -> AttemptId {
        AttemptId::from_uuid(self.next_id())
    }
    /// Fresh [`EvaluationId`].
    fn evaluation_id(&self) -> EvaluationId {
        EvaluationId::from_uuid(self.next_id())
    }
    /// Fresh [`MemoryItemId`].
    fn memory_item_id(&self) -> MemoryItemId {
        MemoryItemId::from_uuid(self.next_id())
    }
    /// Fresh [`EventId`].
    fn event_id(&self) -> EventId {
        EventId::from_uuid(self.next_id())
    }
    /// Fresh [`CommandId`].
    fn command_id(&self) -> CommandId {
        CommandId::from_uuid(self.next_id())
    }
}

/// Random, time-ordered uuid v7 ids from the system clock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UuidV7IdGen;

impl IdGen for UuidV7IdGen {
    fn next_id(&self) -> Uuid {
        Uuid::now_v7()
    }
}

impl<G: IdGen + ?Sized> IdGen for Arc<G> {
    fn next_id(&self) -> Uuid {
        (**self).next_id()
    }
}

impl<G: IdGen + ?Sized> IdGen for &G {
    fn next_id(&self) -> Uuid {
        (**self).next_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_close_to_now() {
        let before = Utc::now();
        let now = SystemClock.now();
        let after = Utc::now();
        assert!(before <= now && now <= after);
    }

    #[test]
    fn uuid_v7_idgen_yields_ordered_v7_ids() {
        let ids = UuidV7IdGen;
        let a = ids.next_id();
        let b = ids.next_id();
        assert_ne!(a, b);
        assert!(a < b);
        assert_eq!(a.get_version(), Some(uuid::Version::SortRand));
        assert!(!ids.run_id().is_nil());
        assert!(!ids.command_id().is_nil());
    }

    fn takes_clock(c: impl Clock) -> DateTime<Utc> {
        c.now()
    }

    #[test]
    fn traits_are_object_safe_and_shareable() {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let ids: Arc<dyn IdGen> = Arc::new(UuidV7IdGen);
        let _ = clock.now();
        let _ = ids.task_id();
        let _ = takes_clock(clock.clone());
        let _ = takes_clock(&*clock);
    }
}
