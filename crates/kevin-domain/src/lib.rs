//! Kevin domain crate — the shared vocabulary (`plan/02-domain-model.md`).
//!
//! Pure types only: identifiers, value objects, the event envelope, the
//! `Clock`/`IdGen` abstractions and (from WS-01 on) the commands, events,
//! aggregates and state machines of every bounded context. **No IO, no tokio,
//! no sqlx** — everything here must be usable from a unit test with nothing but
//! `std`.
//!
//! Dependency direction: `kevin-domain` depends on no other Kevin crate; every
//! other crate depends on it.
//!
//! Module map (frozen names, extend rather than rename):
//! - [`ids`] — uuid v7 newtypes (`RunId`, `TaskId`, …).
//! - [`kinds`] — `TaskKind`, `WorkerKind`, `ModelAlias`, `Effort`, `FailureClass`.
//! - [`envelope`] — `EventEnvelope<E>` and `Actor`.
//! - [`clock`] — `Clock`/`IdGen` traits with the production implementations.

pub mod clock;
pub mod envelope;
pub mod ids;
pub mod kinds;

pub use clock::{Clock, IdGen, SystemClock, UuidV7IdGen};
pub use envelope::{Actor, EventEnvelope};
pub use ids::{
    AttemptId, CommandId, EvaluationId, EventId, MemoryItemId, QuestionId, RunId, TaskId,
};
pub use kinds::{Effort, FailureClass, ModelAlias, TaskKind, WorkerKind};
