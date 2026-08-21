//! Pool construction (`Db::connect`) and connection-level checks.

use std::str::FromStr as _;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::error::StoreError;

/// Default database URL of the laptop profile (`plan/03-config-schema.md` `[database]`).
pub const DEFAULT_URL: &str = "postgres://kevin:kevin@localhost:5432/kevin";

/// The `[database]` section as the store needs it (`plan/03-config-schema.md`),
/// with `url_file` already resolved. Built from `kevin_config::Database` with
/// [`DatabaseCfg::from_config`].
#[derive(Debug, Clone)]
pub struct DatabaseCfg {
    /// `postgres://…` connection URL.
    pub url: String,
    /// Maximum number of pooled connections.
    pub pool_size: u32,
    /// Whether `serve` runs pending migrations itself (startup stage 4).
    pub auto_migrate: bool,
    /// Server-side `statement_timeout` applied to every pooled connection.
    pub statement_timeout: Duration,
}

impl Default for DatabaseCfg {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.to_owned(),
            pool_size: 10,
            auto_migrate: true,
            statement_timeout: Duration::from_secs(30),
        }
    }
}

impl DatabaseCfg {
    /// Laptop defaults with a different URL.
    pub fn with_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Self::default()
        }
    }

    /// Resolves the validated `[database]` config: when `url_file` is set its
    /// (trimmed) contents are the URL; otherwise `url` is used as is.
    pub fn from_config(db: &kevin_config::Database) -> Result<Self, StoreError> {
        let url = match &db.url_file {
            Some(path) => std::fs::read_to_string(path)
                .map_err(|e| {
                    StoreError::InvalidConfig(format!("database.url_file {}: {e}", path.display()))
                })?
                .trim()
                .to_owned(),
            None => db.url.clone(),
        };
        if url.is_empty() {
            return Err(StoreError::InvalidConfig(
                "database.url is empty (set database.url or database.url_file)".into(),
            ));
        }
        Ok(Self {
            url,
            pool_size: db.pool_size,
            auto_migrate: db.auto_migrate,
            statement_timeout: db.statement_timeout,
        })
    }
}

impl TryFrom<&kevin_config::Database> for DatabaseCfg {
    type Error = StoreError;

    fn try_from(db: &kevin_config::Database) -> Result<Self, Self::Error> {
        Self::from_config(db)
    }
}

/// Pool factory.
#[derive(Debug, Clone, Copy)]
pub struct Db;

impl Db {
    /// Connects a pool from the validated `[database]` config section
    /// (`url`/`url_file`, `pool_size`, `statement_timeout`).
    pub async fn connect(db: &kevin_config::Database) -> Result<PgPool, StoreError> {
        Self::connect_with(&DatabaseCfg::from_config(db)?).await
    }

    /// Connects a pool sized and timed out per `cfg`. Fails fast if the first
    /// connection cannot be established.
    pub async fn connect_with(cfg: &DatabaseCfg) -> Result<PgPool, StoreError> {
        if cfg.pool_size == 0 {
            return Err(StoreError::InvalidConfig(
                "database.pool_size must be >= 1".into(),
            ));
        }
        let options = Self::connect_options(cfg)?;
        let pool = PgPoolOptions::new()
            .max_connections(cfg.pool_size)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(options)
            .await?;
        Ok(pool)
    }

    /// Connects with laptop defaults and the given URL.
    pub async fn connect_url(url: &str) -> Result<PgPool, StoreError> {
        Self::connect_with(&DatabaseCfg::with_url(url)).await
    }

    /// Connect options derived from `cfg` (application name, statement timeout).
    pub fn connect_options(cfg: &DatabaseCfg) -> Result<PgConnectOptions, StoreError> {
        if !cfg.url.starts_with("postgres://") && !cfg.url.starts_with("postgresql://") {
            return Err(StoreError::InvalidConfig(format!(
                "database.url must start with postgres:// (got `{}`)",
                redact_url(&cfg.url)
            )));
        }
        let options = PgConnectOptions::from_str(&cfg.url)
            .map_err(|e| StoreError::InvalidConfig(format!("database.url: {e}")))?
            .application_name("kevin");
        let millis = u64::try_from(cfg.statement_timeout.as_millis()).unwrap_or(u64::MAX);
        let options = if millis == 0 {
            options
        } else {
            options.options([("statement_timeout", format!("{millis}ms"))])
        };
        Ok(options)
    }

    /// Fails with [`StoreError::PgVectorMissing`] unless the `vector` extension
    /// is installed in the connected database (startup stage 3).
    pub async fn check_pgvector(pool: &PgPool) -> Result<(), StoreError> {
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')",
        )
        .fetch_one(pool)
        .await?;
        if installed {
            Ok(())
        } else {
            Err(StoreError::PgVectorMissing)
        }
    }
}

/// Masks the password of a `postgres://user:password@host/db` URL.
#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((userinfo, host)) = rest.split_once('@') else {
        return url.to_owned();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_password_only() {
        assert_eq!(
            redact_url("postgres://kevin:s3cret@localhost:5433/kevin"),
            "postgres://kevin:***@localhost:5433/kevin"
        );
        assert_eq!(
            redact_url("postgres://kevin@localhost/kevin"),
            "postgres://kevin@localhost/kevin"
        );
        assert_eq!(redact_url("not a url"), "not a url");
    }

    #[test]
    fn rejects_non_postgres_urls() {
        let cfg = DatabaseCfg::with_url("mysql://x");
        assert!(matches!(
            Db::connect_options(&cfg),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn from_config_reads_url_file_and_rejects_empty() {
        let dir = std::env::temp_dir().join(format!("kevin-store-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("db-url");
        std::fs::write(&file, "postgres://kevin:pw@localhost:5433/kevin\n").unwrap();
        let cfg = DatabaseCfg::from_config(&kevin_config::Database {
            url: String::new(),
            url_file: Some(file.clone()),
            pool_size: 3,
            ..kevin_config::Database::default()
        })
        .unwrap();
        assert_eq!(cfg.url, "postgres://kevin:pw@localhost:5433/kevin");
        assert_eq!(cfg.pool_size, 3);
        let _ = std::fs::remove_dir_all(&dir);
        let empty = kevin_config::Database {
            url: String::new(),
            ..kevin_config::Database::default()
        };
        assert!(matches!(
            DatabaseCfg::from_config(&empty),
            Err(StoreError::InvalidConfig(_))
        ));
        let plain = DatabaseCfg::from_config(&kevin_config::Database::default()).unwrap();
        assert_eq!(plain.url, DEFAULT_URL);
    }

    #[test]
    fn statement_timeout_becomes_a_server_option() {
        let cfg = DatabaseCfg::default();
        let options = Db::connect_options(&cfg).unwrap();
        assert_eq!(options.get_database(), Some("kevin"));
    }
}
