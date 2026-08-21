//! Migrations runner (`plan/10-observability-ops.md` §Migrations and data).
//!
//! Every SQL migration of the workspace lives in `crates/kevin-store/migrations`
//! (see the README there for the numbering scheme) and is embedded in
//! [`MIGRATOR`]. [`migrate`] applies or only checks them depending on the
//! [`MigratePolicy`] (`database.auto_migrate`); [`status`] is what
//! `kevin db status` prints; [`reset`] drops every Kevin schema and re-applies
//! everything (dev only — `kevin db reset --yes`).

use sqlx::migrate::{Migrate as _, Migrator};
use sqlx::{AssertSqlSafe, PgPool};

use crate::error::StoreError;

/// All migrations of the workspace, embedded at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Name of the sqlx bookkeeping table (in `public`).
pub const MIGRATIONS_TABLE: &str = "_sqlx_migrations";

/// Schemas owned by Kevin (`plan/01-architecture.md` §Storage); `reset` drops them.
pub const KEVIN_SCHEMAS: &[&str] = &["core", "orch", "routing", "memory", "eval", "kohral"];

/// What to do about pending migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigratePolicy {
    /// Apply every pending migration (`database.auto_migrate = true`).
    Apply,
    /// Only verify: succeed when everything is applied, fail with
    /// [`StoreError::MigrationsPending`] otherwise (`auto_migrate = false`).
    CheckOnly,
}

/// One embedded migration and whether the database has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationEntry {
    /// Version number (the `NNNN` file prefix).
    pub version: i64,
    /// Description (the rest of the file name).
    pub description: String,
    /// State in the database.
    pub state: MigrationState,
}

/// State of one migration in the target database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationState {
    /// Applied with the same checksum as the embedded file.
    Applied,
    /// Not applied yet.
    Pending,
    /// Applied, but the file changed since (checksum mismatch).
    ChecksumMismatch,
}

/// Result of [`status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// Every embedded migration, in version order.
    pub entries: Vec<MigrationEntry>,
    /// Versions present in the database that this binary does not embed
    /// (a newer binary migrated this database).
    pub unknown_applied: Vec<i64>,
    /// Whether the `vector` extension is installed.
    pub pgvector_installed: bool,
}

impl MigrationStatus {
    /// Versions not applied yet.
    #[must_use]
    pub fn pending(&self) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|e| e.state == MigrationState::Pending)
            .map(|e| e.version)
            .collect()
    }

    /// Versions applied with a different checksum.
    #[must_use]
    pub fn mismatched(&self) -> Vec<i64> {
        self.entries
            .iter()
            .filter(|e| e.state == MigrationState::ChecksumMismatch)
            .map(|e| e.version)
            .collect()
    }

    /// `true` when every embedded migration is applied unchanged and nothing unknown is applied.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.pending().is_empty() && self.mismatched().is_empty() && self.unknown_applied.is_empty()
    }
}

/// Result of [`migrate`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrationReport {
    /// Versions applied by this call (empty when everything was current).
    pub applied: Vec<i64>,
    /// Versions that were already applied before this call.
    pub already_applied: Vec<i64>,
}

/// Computes the migration status of `pool`'s database without changing it.
pub async fn status(pool: &PgPool) -> Result<MigrationStatus, StoreError> {
    let mut conn = pool.acquire().await?;
    let table_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_tables WHERE schemaname = 'public' AND tablename = $1)",
    )
    .bind(MIGRATIONS_TABLE)
    .fetch_one(&mut *conn)
    .await?;
    let applied = if table_exists {
        conn.list_applied_migrations(MIGRATIONS_TABLE).await?
    } else {
        Vec::new()
    };
    let pgvector_installed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(&mut *conn)
            .await?;

    let entries = MIGRATOR
        .iter()
        .map(|m| {
            let state = match applied.iter().find(|a| a.version == m.version) {
                None => MigrationState::Pending,
                Some(a) if a.checksum == m.checksum => MigrationState::Applied,
                Some(_) => MigrationState::ChecksumMismatch,
            };
            MigrationEntry {
                version: m.version,
                description: m.description.to_string(),
                state,
            }
        })
        .collect();
    let unknown_applied = applied
        .iter()
        .filter(|a| !MIGRATOR.version_exists(a.version))
        .map(|a| a.version)
        .collect();
    Ok(MigrationStatus {
        entries,
        unknown_applied,
        pgvector_installed,
    })
}

/// Applies (or, with [`MigratePolicy::CheckOnly`], verifies) the embedded migrations.
///
/// Idempotent: a second call with `Apply` applies nothing and reports every
/// version as `already_applied`. Fails with [`StoreError::MigrationMismatch`]
/// when an applied migration was modified or is unknown to this binary.
pub async fn migrate(pool: &PgPool, policy: MigratePolicy) -> Result<MigrationReport, StoreError> {
    let before = status(pool).await?;
    if let Some(version) = before.mismatched().first() {
        return Err(StoreError::MigrationMismatch { version: *version });
    }
    if let Some(version) = before.unknown_applied.first() {
        return Err(StoreError::MigrationMismatch { version: *version });
    }
    let pending = before.pending();
    match policy {
        MigratePolicy::CheckOnly => {
            if pending.is_empty() {
                Ok(MigrationReport {
                    applied: Vec::new(),
                    already_applied: before.entries.iter().map(|e| e.version).collect(),
                })
            } else {
                Err(StoreError::MigrationsPending { pending })
            }
        }
        MigratePolicy::Apply => {
            MIGRATOR.run(pool).await?;
            Ok(MigrationReport {
                applied: pending,
                already_applied: before
                    .entries
                    .iter()
                    .filter(|e| e.state == MigrationState::Applied)
                    .map(|e| e.version)
                    .collect(),
            })
        }
    }
}

/// Drops every Kevin schema ([`KEVIN_SCHEMAS`]) and the migrations table, then
/// re-applies all migrations. **Destroys all data** — dev only.
pub async fn reset(pool: &PgPool) -> Result<MigrationReport, StoreError> {
    let mut tx = pool.begin().await?;
    for schema in KEVIN_SCHEMAS {
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE"
        )))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(AssertSqlSafe(format!(
        "DROP TABLE IF EXISTS public.{MIGRATIONS_TABLE}"
    )))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    migrate(pool, MigratePolicy::Apply).await
}

/// A stable fingerprint of the embedded migration set (versions + checksums).
/// Test harnesses use it to name template databases so two binaries with
/// different migration sets never share a template.
#[must_use]
pub fn migrations_fingerprint() -> u64 {
    // FNV-1a over "version:checksum" for every migration, in order.
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut feed = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    };
    for m in MIGRATOR.iter() {
        for b in m.version.to_le_bytes() {
            feed(b);
        }
        for &b in m.checksum.iter() {
            feed(b);
        }
        feed(b'|');
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_migration_is_embedded_first() {
        let first = MIGRATOR.iter().next().expect("at least one migration");
        assert_eq!(first.version, 1);
        assert_eq!(first.description, "core");
    }

    #[test]
    fn migration_versions_are_strictly_increasing() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(versions, sorted, "versions must be unique and ordered");
    }

    #[test]
    fn fingerprint_is_stable_within_a_build() {
        assert_eq!(migrations_fingerprint(), migrations_fingerprint());
        assert_ne!(migrations_fingerprint(), 0);
    }
}
