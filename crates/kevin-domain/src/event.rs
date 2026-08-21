//! [`DomainEvent`] — the union of every aggregate's events, for buses,
//! stores and sagas that handle streams of mixed aggregates
//! (`EventEnvelope<DomainEvent>`).
//!
//! Serde is `untagged` over the six internally tagged event enums: because
//! every payload carries its catalog `type` (`run.started`, `task.routed`, …)
//! and the prefixes are disjoint, deserialisation is unambiguous.

use serde::{Deserialize, Serialize};

use crate::aggregate::EventMeta;
use crate::evaluation::EvaluationEvent;
use crate::memory_item::MemoryItemEvent;
use crate::question::QuestionEvent;
use crate::route_score::RouteScoreEvent;
use crate::run::RunEvent;
use crate::task::TaskEvent;

/// Any event of any aggregate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DomainEvent {
    /// `run.*`
    Run(RunEvent),
    /// `task.*`
    Task(TaskEvent),
    /// `question.*`
    Question(QuestionEvent),
    /// `evaluation.*`
    Evaluation(EvaluationEvent),
    /// `routing.*`
    RouteScore(RouteScoreEvent),
    /// `memory.*`
    MemoryItem(MemoryItemEvent),
}

impl DomainEvent {
    /// Every event type in the v1 catalog, grouped by aggregate.
    #[must_use]
    pub fn catalog() -> Vec<&'static str> {
        RunEvent::TYPES
            .iter()
            .chain(TaskEvent::TYPES.iter())
            .chain(QuestionEvent::TYPES.iter())
            .chain(EvaluationEvent::TYPES.iter())
            .chain(RouteScoreEvent::TYPES.iter())
            .chain(MemoryItemEvent::TYPES.iter())
            .copied()
            .collect()
    }

    /// The inner run event, if any.
    #[must_use]
    pub const fn as_run(&self) -> Option<&RunEvent> {
        match self {
            DomainEvent::Run(e) => Some(e),
            _ => None,
        }
    }

    /// The inner task event, if any.
    #[must_use]
    pub const fn as_task(&self) -> Option<&TaskEvent> {
        match self {
            DomainEvent::Task(e) => Some(e),
            _ => None,
        }
    }

    /// The inner question event, if any.
    #[must_use]
    pub const fn as_question(&self) -> Option<&QuestionEvent> {
        match self {
            DomainEvent::Question(e) => Some(e),
            _ => None,
        }
    }

    /// The inner evaluation event, if any.
    #[must_use]
    pub const fn as_evaluation(&self) -> Option<&EvaluationEvent> {
        match self {
            DomainEvent::Evaluation(e) => Some(e),
            _ => None,
        }
    }

    /// The inner route-score event, if any.
    #[must_use]
    pub const fn as_route_score(&self) -> Option<&RouteScoreEvent> {
        match self {
            DomainEvent::RouteScore(e) => Some(e),
            _ => None,
        }
    }

    /// The inner memory-item event, if any.
    #[must_use]
    pub const fn as_memory_item(&self) -> Option<&MemoryItemEvent> {
        match self {
            DomainEvent::MemoryItem(e) => Some(e),
            _ => None,
        }
    }
}

impl EventMeta for DomainEvent {
    fn event_type(&self) -> &'static str {
        match self {
            DomainEvent::Run(e) => e.event_type(),
            DomainEvent::Task(e) => e.event_type(),
            DomainEvent::Question(e) => e.event_type(),
            DomainEvent::Evaluation(e) => e.event_type(),
            DomainEvent::RouteScore(e) => e.event_type(),
            DomainEvent::MemoryItem(e) => e.event_type(),
        }
    }

    fn schema_version(&self) -> u16 {
        match self {
            DomainEvent::Run(e) => e.schema_version(),
            DomainEvent::Task(e) => e.schema_version(),
            DomainEvent::Question(e) => e.schema_version(),
            DomainEvent::Evaluation(e) => e.schema_version(),
            DomainEvent::RouteScore(e) => e.schema_version(),
            DomainEvent::MemoryItem(e) => e.schema_version(),
        }
    }

    fn aggregate_type(&self) -> &'static str {
        match self {
            DomainEvent::Run(e) => e.aggregate_type(),
            DomainEvent::Task(e) => e.aggregate_type(),
            DomainEvent::Question(e) => e.aggregate_type(),
            DomainEvent::Evaluation(e) => e.aggregate_type(),
            DomainEvent::RouteScore(e) => e.aggregate_type(),
            DomainEvent::MemoryItem(e) => e.aggregate_type(),
        }
    }
}

macro_rules! event_from {
    ($($variant:ident($ty:ty)),* $(,)?) => {
        $(impl From<$ty> for DomainEvent {
            fn from(event: $ty) -> Self {
                DomainEvent::$variant(event)
            }
        })*
    };
}

event_from!(
    Run(RunEvent),
    Task(TaskEvent),
    Question(QuestionEvent),
    Evaluation(EvaluationEvent),
    RouteScore(RouteScoreEvent),
    MemoryItem(MemoryItemEvent),
);
