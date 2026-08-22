//! Kevin domain crate — the shared vocabulary (`plan/02-domain-model.md`).
//!
//! Pure types only: identifiers, value objects, the event envelope, the
//! `Clock`/`IdGen` abstractions, and the commands, events, aggregates and
//! state machines of every bounded context. **No IO, no tokio, no sqlx** —
//! everything here must be usable from a unit test with nothing but `std`.
//!
//! Dependency direction: `kevin-domain` depends on no other Kevin crate; every
//! other crate depends on it.
//!
//! Module map (frozen names, extend rather than rename):
//! - [`ids`] — uuid v7 newtypes (`RunId`, `TaskId`, …).
//! - [`kinds`] — `TaskKind`, `WorkerKind`, `ModelAlias`, `Effort`, `FailureClass`, `Complexity`, `Tier`.
//! - [`envelope`] — `EventEnvelope<E>` and `Actor`.
//! - [`clock`] — `Clock`/`IdGen` traits with the production implementations.
//! - [`values`] — `Route`, `Budget`, `Usage`, `Workspace`, `ArtifactRef`, `Goal`, `TaskSpec`, …
//! - [`understanding`] / [`plan`] — `kevin.understanding.v1` / `kevin.plan.v1` shapes, `PlanValidator`.
//! - [`aggregate`] — the `Aggregate` and `EventMeta` traits.
//! - [`error`] — `DomainError`.
//! - [`run`], [`task`], [`question`], [`evaluation`], [`route_score`], [`memory_item`] — aggregates.
//! - [`event`] — `DomainEvent`, the union of every aggregate's events.
//!
//! Event payloads are serialised internally tagged on `type` with their
//! catalog name (`{"type":"run.started", …}`), so a stored payload is
//! self-describing and `DomainEvent` deserialises without the envelope.

pub mod aggregate;
pub mod clock;
pub mod envelope;
pub mod error;
pub mod evaluation;
pub mod event;
pub mod ids;
pub mod kinds;
pub mod memory_item;
pub mod plan;
pub mod question;
pub mod route_score;
pub mod run;
pub mod task;
pub mod understanding;
pub mod values;

pub use aggregate::{Aggregate, EventMeta};
pub use clock::{Clock, IdGen, SystemClock, UuidV7IdGen};
pub use envelope::{Actor, EventEnvelope};
pub use error::DomainError;
pub use evaluation::{Evaluation, EvaluationCommand, EvaluationEvent};
pub use event::DomainEvent;
pub use ids::{
    ArtifactId, AttemptId, CommandId, EvaluationId, EventId, MemoryItemId, ProposalId, QuestionId,
    RunId, TaskId,
};
pub use kinds::{Complexity, Effort, FailureClass, ModelAlias, TaskKind, Tier, WorkerKind};
pub use memory_item::{MemoryItem, MemoryItemCommand, MemoryItemEvent};
pub use plan::{Plan, PlanEdge, PlanError, PlanTask, PlanValidator};
pub use question::{Question, QuestionCommand, QuestionEvent};
pub use route_score::{RouteScore, RouteScoreCommand, RouteScoreEvent, RouteStats};
pub use run::{RoleOverrides, Run, RunCommand, RunEvent, RunStatus};
pub use rust_decimal::Decimal;
pub use task::{Attempt, Task, TaskCommand, TaskEvent, TaskStatus};
pub use understanding::{ProposedQuestion, Understanding};
pub use values::{
    Answer, ArtifactKind, ArtifactRef, Budget, BudgetDimension, BudgetExcess, EvaluationSubject,
    Goal, JsonSchema, MemoryKind, MemoryScope, MemorySource, Proposal, ProposalKind,
    ProposalStatus, QuestionOption, QuestionPolicy, QuestionStatus, RepoKind, Route, RubricScore,
    RunFailureReason, RunMode, TaskSpec, Usage, Verdict, Workspace, WorkspaceKind, WorkspacePolicy,
};
