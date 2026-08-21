//! Postgres platform crate (`plan/01-architecture.md` §Storage, `plan/02` §Event envelope).
//!
//! Owns the sqlx Postgres access layer: event store (append with optimistic
//! concurrency, stream/global reads), outbox relay, snapshots, processed
//! commands (idempotency), projection checkpoints, migrations runner and
//! pgvector helpers. Schema `core.*`.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`; never on orchestration or interface crates. Implemented
//! by WS-03 (migrations in `crates/kevin-store/migrations/`).
