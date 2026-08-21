//! Orchestration core context (`plan/05-orchestration.md`).
//!
//! Owns the application services (`RunService`, `TaskService`,
//! `QuestionService`), the `RunSupervisor`/`RunActor` process manager and its
//! saga, the `TaskRunner` (folding worker streams into domain events), the
//! scheduler, budgets, retries, shutdown/restart semantics, role prompt
//! builders (`roles/`) and projections (`projections/`). Schema `orch.*`.
//!
//! Dependency direction: depends on every core, supporting and platform crate;
//! nothing below it depends on it. Only interface crates (`kevin-api`,
//! `kevin-kohral`, `kevin-cli`) depend on it. Implemented by WS-08 (engine),
//! WS-10 (roles), WS-11 (projections).
//!
//! # Module map (WS-08)
//!
//! - [`error`] — [`AppError`], the failure type every service returns.
//! - [`ports`] — the traits the engine depends on: [`ports::RouterPort`]
//!   (WS-09), [`ports::RolesPort`] (implemented here over WS-10's
//!   `RoleRunner`), [`ports::MemoryPort`] (WS-18) and
//!   [`ports::EvaluatorPort`] (WS-19), plus the workspace, idempotency and
//!   system-context seams. WS-12 wires the real crates.
//! - [`local_workspace`] — the production [`ports::WorkspacePort`] over
//!   `kevin-workspace` (git worktrees / jj workspaces, `gh` PRs).
//! - [`role_port`] — the production [`ports::RolesPort`] over WS-10's
//!   [`roles::RoleRunner`].
//! - [`services`] — thin command handlers: load stream → rehydrate → `handle`
//!   → append with OCC → publish.
//! - [`scheduler`] — topological ready-set and the concurrency bulkheads.
//! - [`task_runner`] — per-attempt state machine folding `WorkerEvent`s.
//! - [`run_actor`] — [`run_actor::RunSupervisor`] and [`run_actor::RunActor`]
//!   (the `RunSaga` of `plan/02-domain-model.md` §Process manager).
//! - [`orchestrator`] — [`orchestrator::Orchestrator::boot`] and its
//!   [`orchestrator::Handle`] (startup/shutdown of `plan/10`).
//! - [`testing`] — in-process fakes of every port, for tests of this crate and
//!   of the crates above it.

pub mod convert;
pub mod error;
pub mod local_workspace;
pub mod orchestrator;
pub mod ports;
pub mod projections;
pub mod role_port;
pub mod roles;
pub mod run_actor;
pub mod scheduler;
pub mod services;
pub mod task_runner;
pub mod testing;

pub use error::AppError;
pub use local_workspace::LocalWorkspace;
pub use orchestrator::{Deps, Handle, Orchestrator};
pub use ports::{EvaluatorPort, MemoryPort, RolesPort, RouterPort};
pub use role_port::RoleRunnerRoles;
pub use run_actor::{RunSupervisor, SagaInput};
pub use services::{QuestionService, RunService, TaskService};
