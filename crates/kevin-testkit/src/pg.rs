//! Postgres test harness (`plan/11-testing.md` §Postgres in tests), behind the
//! `pg` feature.
//!
//! [`TestDb::new()`] gives every test its own database cloned from a template
//! that already has every migration applied:
//!
//! 1. The admin URL comes from `DATABASE_URL` (default
//!    `postgres://kevin:kevin@localhost:5433/kevin`, the compose file's port).
//!    Tests start with `kevin_testkit::skip_unless_pg!()` so they skip with a
//!    message where no Postgres is configured (CI's macOS job).
//!    Tests never create objects in that database; it is only used to run
//!    `CREATE DATABASE`.
//! 2. Once per process (and once per migration set across processes, guarded by
//!    a Postgres advisory lock) the template `kevin_tpl_<fingerprint>` is
//!    created and migrated with `kevin_store::MIGRATOR`. The fingerprint covers
//!    every embedded migration's version and checksum, so workspaces with
//!    different migration sets never share a template.
//! 3. Each `TestDb` is `CREATE DATABASE kevin_test_<unix-secs>_<hex> TEMPLATE …`.
//!    It is dropped with `DROP DATABASE … WITH (FORCE)` by [`TestDb::close`]
//!    or, failing that, by `Drop` (on a helper thread, so it also runs when a
//!    test panics). Leftovers (killed processes) older than one hour — by the
//!    timestamp in their name — are swept when the next process creates its
//!    template.
//!
//! No testcontainers: the project's compose file / CI service container provide
//! the server (`plan/11` "CI override"), which keeps parallel agents and
//! nextest processes on one shared Postgres without collisions.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use kevin_store::migrate::migrations_fingerprint;
use kevin_store::{DatabaseCfg, Db, MIGRATOR, PgPool};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgConnectOptions;
use tokio::sync::Mutex;

/// Default admin URL when `DATABASE_URL` is unset.
pub const DEFAULT_DATABASE_URL: &str = "postgres://kevin:kevin@localhost:5433/kevin";

/// Prefix of per-test databases.
pub const TEST_DB_PREFIX: &str = "kevin_test_";

/// Prefix of template databases (suffix = migrations fingerprint).
pub const TEMPLATE_DB_PREFIX: &str = "kevin_tpl_";

/// Advisory lock key taken (on the admin database) while creating a template.
const TEMPLATE_LOCK_KEY: i64 = 0x4b45_5649_4e5f_5450; // "KEVIN_TP"

/// Leftover test databases older than this (by name timestamp) are dropped.
const STALE_AFTER_SECS: u64 = 3600;

/// A fresh, migrated database for one test.
#[derive(Debug)]
pub struct TestDb {
    name: String,
    url: String,
    admin_url: String,
    pool: PgPool,
    dropped: bool,
}

impl TestDb {
    /// Creates a fresh database from the migrated template and connects a pool to it.
    ///
    /// # Panics
    /// Panics when Postgres is unreachable or the template cannot be created —
    /// the test cannot run without it, and a panic names the reason.
    pub async fn new() -> Self {
        Self::try_new().await.unwrap_or_else(|e| {
            panic!(
                "TestDb::new: {e} (is Postgres up? `just db-up`; DATABASE_URL={})",
                admin_url()
            )
        })
    }

    /// Fallible [`Self::new`].
    pub async fn try_new() -> Result<Self, sqlx::Error> {
        let admin_url = admin_url();
        let template = ensure_template(&admin_url).await?;
        let name = fresh_name();
        {
            let admin = admin_pool(&admin_url).await?;
            sqlx::query(AssertSqlSafe(format!(
                "CREATE DATABASE {name} TEMPLATE {template}"
            )))
            .execute(&admin)
            .await?;
        }
        let url = with_database(&admin_url, &name);
        let pool = Db::connect_with(&DatabaseCfg {
            url: url.clone(),
            pool_size: 8,
            ..DatabaseCfg::default()
        })
        .await
        .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
        Ok(Self {
            name,
            url,
            admin_url,
            pool,
            dropped: false,
        })
    }

    /// Pool connected to the test database.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Connection URL of the test database (for CLI invocations or a second pool).
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Database name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Admin URL (`DATABASE_URL`) used to create/drop databases.
    #[must_use]
    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    /// A second, independent pool to the same database — simulates another process.
    pub async fn connect_other(&self) -> PgPool {
        Db::connect_url(&self.url)
            .await
            .unwrap_or_else(|e| panic!("TestDb::connect_other: {e}"))
    }

    /// Closes the pool and drops the database now (deterministic clean-up).
    ///
    /// `Pool::close` waits for checked-out connections (e.g. a `PgListener`
    /// built with `connect_with(pool)`): drop those first. After 10 s the
    /// database is force-dropped anyway.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), self.pool.close()).await;
        if let Err(e) = drop_database(&self.admin_url, &self.name).await {
            eprintln!("TestDb::close: dropping {} failed: {e}", self.name);
        }
        self.dropped = true;
    }
}

impl Drop for TestDb {
    /// Drops the database even when the test forgot [`TestDb::close`] or
    /// panicked: the work runs on a dedicated thread with its own small
    /// runtime (a task spawned on the test runtime would be cancelled when
    /// the test returns), and the current thread waits for it.
    fn drop(&mut self) {
        if self.dropped {
            return;
        }
        let pool = self.pool.clone();
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let worker = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), pool.close()).await;
                if let Err(e) = drop_database(&admin_url, &name).await {
                    eprintln!("TestDb: dropping {name} failed: {e}");
                }
            });
        });
        let _ = worker.join();
    }
}

/// Whether Postgres-backed tests should run: `DATABASE_URL` is set and
/// non-empty (`just test` exports it; CI's macOS job leaves it unset and the
/// tests skip — see [`skip_unless_pg!`](crate::skip_unless_pg)).
#[must_use]
pub fn available() -> bool {
    std::env::var("DATABASE_URL").is_ok_and(|s| !s.trim().is_empty())
}

/// Returns from the current test with a "skipped" message when
/// [`available`](crate::pg::available) is false. Put it first in every
/// Postgres-backed test.
#[macro_export]
macro_rules! skip_unless_pg {
    () => {
        if !$crate::pg::available() {
            eprintln!(
                "skipping {}: DATABASE_URL is not set (Postgres-backed test)",
                module_path!()
            );
            return;
        }
    };
}

/// `DATABASE_URL` or the default.
#[must_use]
pub fn admin_url() -> String {
    std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DATABASE_URL.to_owned())
}

/// Replaces the database part of `url` with `database`.
#[must_use]
pub fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    // scheme://userinfo@host[:port]/db  → strip anything after the last '/' past the host.
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let after_scheme = &base[scheme_end..];
    let path_start = after_scheme
        .find('/')
        .map_or(base.len(), |i| scheme_end + i);
    let mut out = format!("{}/{database}", &base[..path_start]);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// Name of the template database for the embedded migration set.
#[must_use]
pub fn template_name() -> String {
    format!("{TEMPLATE_DB_PREFIX}{:016x}", migrations_fingerprint())
}

/// Drops `name` with `WITH (FORCE)` through the admin database.
pub async fn drop_database(admin_url: &str, name: &str) -> Result<(), sqlx::Error> {
    let admin = admin_pool(admin_url).await?;
    sqlx::query(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await?;
    Ok(())
}

fn fresh_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    format!("{TEST_DB_PREFIX}{secs}_{}", &suffix[suffix.len() - 12..])
}

async fn admin_pool(admin_url: &str) -> Result<PgPool, sqlx::Error> {
    let options: PgConnectOptions = admin_url.parse()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options.application_name("kevin-testkit"))
        .await
}

/// Creates (once per process, and once per migration set across processes)
/// the migrated template database; returns its name.
async fn ensure_template(admin_url: &str) -> Result<String, sqlx::Error> {
    static TEMPLATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    let slot = TEMPLATE.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().await;
    if let Some(name) = guard.as_ref() {
        return Ok(name.clone());
    }
    let name = template_name();
    let admin = admin_pool(admin_url).await?;
    // Hold the cross-process lock on a dedicated connection for the whole job.
    let mut lock_conn = admin.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await?;
    let result = build_template(&admin, admin_url, &name).await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_LOCK_KEY)
        .execute(&mut *lock_conn)
        .await?;
    drop(lock_conn);
    result?;
    *guard = Some(name.clone());
    Ok(name)
}

async fn build_template(admin: &PgPool, admin_url: &str, name: &str) -> Result<(), sqlx::Error> {
    sweep_stale(admin).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(name)
            .fetch_one(admin)
            .await?;
    if exists {
        return Ok(());
    }
    // Build under a scratch name, migrate, then rename: a crash mid-way never
    // leaves a half-migrated database under the template's name.
    let scratch = format!("{name}_building");
    sqlx::query(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {scratch} WITH (FORCE)"
    )))
    .execute(admin)
    .await?;
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {scratch}")))
        .execute(admin)
        .await?;
    {
        let options: PgConnectOptions = with_database(admin_url, &scratch).parse()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))?;
        pool.close().await;
    }
    sqlx::query(AssertSqlSafe(format!(
        "ALTER DATABASE {scratch} RENAME TO {name}"
    )))
    .execute(admin)
    .await?;
    sqlx::query(AssertSqlSafe(format!(
        "ALTER DATABASE {name} WITH IS_TEMPLATE true ALLOW_CONNECTIONS false"
    )))
    .execute(admin)
    .await?;
    Ok(())
}

/// Drops `kevin_test_<secs>_…` databases whose timestamp is older than an hour.
async fn sweep_stale(admin: &PgPool) -> Result<(), sqlx::Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database WHERE datname LIKE $1 AND NOT datistemplate",
    )
    .bind(format!("{TEST_DB_PREFIX}%"))
    .fetch_all(admin)
    .await?;
    for name in names {
        let secs = name
            .trim_start_matches(TEST_DB_PREFIX)
            .split('_')
            .next()
            .and_then(|s| s.parse::<u64>().ok());
        if secs.is_some_and(|s| now.saturating_sub(s) > STALE_AFTER_SECS) {
            let _ = sqlx::query(AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
            )))
            .execute(admin)
            .await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_database_replaces_the_path() {
        assert_eq!(
            with_database("postgres://kevin:kevin@localhost:5433/kevin", "x"),
            "postgres://kevin:kevin@localhost:5433/x"
        );
        assert_eq!(
            with_database(
                "postgres://kevin:kevin@localhost:5433/kevin?sslmode=disable",
                "x"
            ),
            "postgres://kevin:kevin@localhost:5433/x?sslmode=disable"
        );
        assert_eq!(
            with_database("postgres://localhost", "x"),
            "postgres://localhost/x"
        );
    }

    #[test]
    fn fresh_names_are_unique_and_prefixed() {
        let a = fresh_name();
        let b = fresh_name();
        assert_ne!(a, b);
        assert!(a.starts_with(TEST_DB_PREFIX));
        assert!(template_name().starts_with(TEMPLATE_DB_PREFIX));
    }
}
