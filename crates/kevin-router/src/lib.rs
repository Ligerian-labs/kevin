//! Routing core context (`plan/06-memory-and-learning.md` §Routing).
//!
//! Owns the model catalog materialised from config, the task-kind taxonomy
//! bindings, `RouteScore` statistics per `(task kind, model alias)`, the
//! selection policy (`fixed`, Thompson sampling) and the price table / cost
//! model. Schema `routing.*`.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry` (and `kevin-store` for persistence, added by WS-09).
//! Implemented by WS-09.
