//! Postgres test harness (WS-03): `TestDb::new().await` creates a database from
//! a migrated template using `DATABASE_URL` (CI service / `just db-up`) or
//! testcontainers, and drops it on success. Tests never touch the `kevin`
//! database itself.
//!
//! Stub: implemented by WS-03.
