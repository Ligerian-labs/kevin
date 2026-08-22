//! Postgres platform crate (`plan/01-architecture.md` §Storage, `plan/02` §Event envelope).
//!
//! Owns the sqlx Postgres access layer of Kevin, schema `core.*`:
//!
//! - [`admin`] — `kevin db init` provisioning (role, database, `vector`).
//! - [`db`] — [`Db::connect`] builds the pool from `kevin_config::Database`
//!   (`url`/`url_file`, pool size, statement timeout; [`DatabaseCfg`] is the
//!   resolved form) and checks pgvector.
//! - [`migrate`] — [`MIGRATOR`] embeds `crates/kevin-store/migrations/**`
//!   (shared by every workstream, see the README there); [`migrate()`] applies
//!   or verifies per [`MigratePolicy`]; [`migrate::status`] / [`migrate::reset`]
//!   back `kevin db status|reset`.
//! - [`event_store`] — the frozen [`EventStore`] trait and [`PgEventStore`]:
//!   append with optimistic concurrency, stream and global reads, position
//!   watch, transactional outbox rows and `pg_notify('kevin_events', …)`.
//!   [`PgEventStore`] also implements `kevin_bus::EventSource`, so
//!   `PgNotifyBus` can read every event back by global position and a second
//!   process can attach to a running instance.
//! - [`outbox`] — [`Outbox`] relay (at-least-once, crash-safe).
//! - [`command_log`] — [`CommandLog`] idempotent command replay.
//! - [`checkpoints`] — [`Checkpoints`] for projections.
//! - [`snapshots`] — [`Snapshots`] per stream.
//! - [`upcast`] — [`Upcasters`] applied on every read.
//!
//! The store works on `EventEnvelope<serde_json::Value>`; typed events are the
//! domain crate's business. Dependency direction: depends on `kevin-domain`,
//! `kevin-config`, `kevin-telemetry`; never on orchestration or interface
//! crates. Implemented by WS-03.

pub mod admin;
pub mod checkpoints;
pub mod command_log;
pub mod db;
pub mod error;
pub mod event_store;
pub mod migrate;
pub mod outbox;
pub mod snapshots;
pub mod upcast;

pub use checkpoints::Checkpoints;
pub use command_log::{Begun, CommandLog, CompleteOutcome, InFlightCommand};
pub use db::{DatabaseCfg, Db};
pub use error::StoreError;
pub use event_store::{
    APPEND_LOCK_KEY, AppendResult, EventStore, NOTIFY_CHANNEL, NewEvent, PgEventStore, StoredEvent,
    StreamId,
};
pub use migrate::{MIGRATOR, MigratePolicy, MigrationReport, MigrationStatus, migrate};
pub use outbox::{DeliveryError, Outbox, RelayReport};
pub use snapshots::{Snapshot, Snapshots};
pub use sqlx::PgPool;
pub use upcast::Upcasters;
