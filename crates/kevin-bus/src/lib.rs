//! Event bus platform crate (`plan/01-architecture.md` §Event-driven core).
//!
//! Owns the `EventBus` trait, the in-process `InProcBus`
//! (`tokio::sync::broadcast`) and the cross-process `PgNotifyBus`
//! (Postgres `LISTEN/NOTIFY` wake-ups with catch-up from the event store).
//! Lag is reported (`Lagged{from,to}`), never silently dropped.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry` (and `kevin-store` for catch-up, added by WS-04).
//! Implemented by WS-04.
