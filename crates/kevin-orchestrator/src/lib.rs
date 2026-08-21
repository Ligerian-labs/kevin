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

pub mod projections;
pub mod roles;
