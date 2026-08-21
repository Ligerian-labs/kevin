//! Database provisioning for `kevin db init`: create role, database and the
//! `vector` extension when the connection has the privileges, otherwise
//! explain what to run by hand.

use std::str::FromStr as _;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};

use crate::db::redact_url;
use crate::error::StoreError;

/// What `init` did (or found already done).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitReport {
    /// Role name of the target URL.
    pub role: String,
    /// Database name of the target URL.
    pub database: String,
    /// Whether the role was created by this call.
    pub role_created: bool,
    /// Whether the database was created by this call.
    pub database_created: bool,
    /// Whether the `vector` extension was created by this call.
    pub extension_created: bool,
    /// Statements the caller must run as a superuser because this connection
    /// lacked the privilege (empty when everything succeeded).
    pub manual_steps: Vec<String>,
}

/// Options of [`init`].
#[derive(Debug, Clone)]
pub struct InitOptions {
    /// `postgres://…` URL Kevin will use (role, password, database are read from it).
    pub target_url: String,
    /// Privileged URL used to create role/database. Defaults to the target URL
    /// pointed at the `postgres` maintenance database.
    pub admin_url: Option<String>,
    /// Also create the role (with the target URL's password) when missing.
    pub create_role: bool,
}

/// Ensures role (optional), database and the `vector` extension exist, and
/// returns a pool connected to the target database.
///
/// Privilege errors are **not** fatal: they are collected as `manual_steps`
/// (SQL to run as a superuser) and the function still tries to connect to the
/// target database; if that fails the error is returned.
pub async fn init(opts: &InitOptions) -> Result<(InitReport, Option<PgPool>), StoreError> {
    let target = PgConnectOptions::from_str(&opts.target_url)
        .map_err(|e| StoreError::InvalidConfig(format!("database.url: {e}")))?;
    let database = target.get_database().unwrap_or("kevin").to_owned();
    let role = target.get_username().to_owned();
    let password = password_of(&opts.target_url);
    let mut report = InitReport {
        role: role.clone(),
        database: database.clone(),
        ..InitReport::default()
    };
    let mut ident_errors = Vec::new();
    if !is_safe_identifier(&role) {
        ident_errors.push(format!("role `{role}`"));
    }
    if !is_safe_identifier(&database) {
        ident_errors.push(format!("database `{database}`"));
    }
    if !ident_errors.is_empty() {
        return Err(StoreError::InvalidConfig(format!(
            "{} must match [a-z_][a-z0-9_]* for `kevin db init`",
            ident_errors.join(" and ")
        )));
    }

    let admin_options = match &opts.admin_url {
        Some(url) => PgConnectOptions::from_str(url)
            .map_err(|e| StoreError::InvalidConfig(format!("--admin-url: {e}")))?,
        None => target.clone().database("postgres"),
    };
    match PgPoolOptions::new()
        .max_connections(1)
        .connect_with(admin_options)
        .await
    {
        Ok(admin) => {
            if opts.create_role {
                let exists: bool =
                    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                        .bind(&role)
                        .fetch_one(&admin)
                        .await?;
                if !exists {
                    let stmt = match &password {
                        Some(pw) => format!(
                            "CREATE ROLE {role} LOGIN PASSWORD '{}'",
                            pw.replace('\'', "''")
                        ),
                        None => format!("CREATE ROLE {role} LOGIN"),
                    };
                    match sqlx::query(AssertSqlSafe(stmt)).execute(&admin).await {
                        Ok(_) => report.role_created = true,
                        Err(e) if is_privilege_error(&e) => report
                            .manual_steps
                            .push(format!("CREATE ROLE {role} LOGIN PASSWORD '<password>';")),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
                    .bind(&database)
                    .fetch_one(&admin)
                    .await?;
            if !exists {
                let stmt = format!("CREATE DATABASE {database} OWNER {role}");
                match sqlx::query(AssertSqlSafe(stmt.clone()))
                    .execute(&admin)
                    .await
                {
                    Ok(_) => report.database_created = true,
                    Err(e) if is_privilege_error(&e) => {
                        report.manual_steps.push(format!("{stmt};"));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            admin.close().await;
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot connect to the admin database; skipping role/database creation");
            report.manual_steps.push(format!(
                "-- could not connect as admin ({}): {e}\nCREATE DATABASE {database} OWNER {role};",
                redact_url(opts.admin_url.as_deref().unwrap_or(&opts.target_url))
            ));
        }
    }

    let pool = match PgPoolOptions::new()
        .max_connections(2)
        .connect_with(target.application_name("kevin"))
        .await
    {
        Ok(pool) => pool,
        Err(e) => {
            if report.manual_steps.is_empty() {
                return Err(e.into());
            }
            return Ok((report, None));
        }
    };
    let has_vector: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(&pool)
            .await?;
    if !has_vector {
        match sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&pool)
            .await
        {
            Ok(_) => report.extension_created = true,
            Err(e) if is_privilege_error(&e) => report.manual_steps.push(format!(
                "\\c {database}\nCREATE EXTENSION IF NOT EXISTS vector;"
            )),
            Err(e) => return Err(e.into()),
        }
    }
    Ok((report, Some(pool)))
}

/// Extracts the password from `postgres://user:password@…`.
fn password_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let userinfo = rest.split_once('@')?.0;
    let pw = userinfo.split_once(':')?.1;
    Some(percent_decode(pw))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_safe_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `42501 insufficient_privilege`.
fn is_privilege_error(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code().map(|c| c == "42501"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_extracted_and_decoded() {
        assert_eq!(
            password_of("postgres://kevin:s%40cret@localhost/kevin").as_deref(),
            Some("s@cret")
        );
        assert_eq!(password_of("postgres://kevin@localhost/kevin"), None);
    }

    #[test]
    fn identifiers_are_validated() {
        assert!(is_safe_identifier("kevin"));
        assert!(is_safe_identifier("kevin_test_1"));
        assert!(!is_safe_identifier("Kevin"));
        assert!(!is_safe_identifier("x;drop"));
        assert!(!is_safe_identifier(""));
    }
}
