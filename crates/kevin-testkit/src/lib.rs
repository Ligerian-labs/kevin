//! Test kit for the Kevin workspace (`plan/11-testing.md`).
//!
//! **Dev-dependency only.** Add it as
//! `[dev-dependencies] kevin-testkit.workspace = true`; production crates must
//! never depend on it (it will grow heavy dependencies such as testcontainers).
//!
//! Module map (stubs are filled in by the owning workstream):
//! - [`clock`] — `FixedClock`, `SeqIdGen` (WS-00, implemented here).
//! - [`pg`] — `TestDb` per-test Postgres databases from a template (WS-03; feature `pg`).
//! - [`fake_worker`] — fake worker scenarios and helpers (WS-05).
//! - [`fake_api`] — in-process fake of the HTTP API for client/TUI tests
//!   (WS-16; feature `api`).
//! - [`given_when_then`] — aggregate given/when/then helpers (WS-01).
//! - [`bus`] — `VecEventSource` (in-memory `EventSource`) and envelope builders (WS-04).

pub mod bus;
pub mod clock;
#[cfg(feature = "api")]
pub mod fake_api;
pub mod fake_worker;
pub mod given_when_then;
#[cfg(feature = "pg")]
pub mod pg;

pub use clock::{FakeClock, FixedClock, SeqIdGen};
