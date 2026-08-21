//! `kevin db …` — database lifecycle (`plan/10-observability-ops.md` §Migrations
//! and data). Owned by WS-03.
//!
//! The database comes from the resolved configuration (`kevin_config::load`
//! through `cmd::config::load_from_ctx`: files, `KEVIN__DATABASE__URL`,
//! `--set database.url=…`); `--url` overrides it for one invocation. The
//! profile guarding `reset` is `kevin.profile` from the same configuration.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Args as _;
use kevin_config::Profile;
use kevin_orchestrator::projections::{
    self, ProjectionError, RebuildReport,
};
use kevin_store::admin::{InitOptions, init};
use kevin_store::db::redact_url;
use kevin_store::migrate::{self, MigratePolicy, MigrationState};
use kevin_store::{DatabaseCfg, Db, EventStore, Outbox, PgEventStore, StoreError};

use crate::cmd::config::load_from_ctx;
use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "db";

/// Arguments of `kevin db`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Database URL for this invocation (default: `database.url` / `database.url_file`
    /// from the resolved configuration, e.g. `KEVIN__DATABASE__URL` or `--set database.url=…`).
    #[arg(long, value_name = "URL", global = true)]
    pub url: Option<String>,
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin db` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Create role/database/extension `vector` (when privileges allow), then migrate.
    Init {
        /// Also create the database role (password taken from the URL).
        #[arg(long)]
        create_role: bool,
        /// Admin connection URL used to create role/database (default: the
        /// database URL pointed at the `postgres` maintenance database).
        #[arg(long, value_name = "URL")]
        admin_url: Option<String>,
    },
    /// Apply pending migrations.
    Migrate,
    /// Show migration status (exit 0 when current, 1 when pending/mismatched).
    Status,
    /// Drop every Kevin schema and re-apply all migrations (laptop profile only).
    Reset {
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Prune old data (delivered outbox rows and task log lines).
    Prune {
        /// Delete delivered outbox rows older than this many days.
        #[arg(long, default_value_t = 7, value_name = "DAYS")]
        outbox_days: u32,
        /// Delete `orch.task_log` lines older than this many days.
        #[arg(long, default_value_t = 30, value_name = "DAYS")]
        task_log_days: u32,
    },
    /// Rebuild a projection from the event store.
    #[command(name = "rebuild-projection")]
    RebuildProjection {
        /// Projection name (one of `orch`'s read models).
        #[arg(value_name = "NAME", required_unless_present = "all", conflicts_with = "all")]
        name: Option<String>,
        /// Rebuild every projection.
        #[arg(long)]
        all: bool,
    },
}

/// The `kevin db` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME))
        .about("Database lifecycle: init, migrate, status, reset, prune, rebuild-projection")
}

/// Runs `kevin db`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let (url, profile) = resolve(args.url.as_deref(), ctx)?;
    match args.cmd {
        Cmd::Init {
            create_role,
            admin_url,
        } => run_init(&url, admin_url, create_role, ctx).await,
        Cmd::Migrate => run_migrate(&url, ctx).await,
        Cmd::Status => run_status(&url, ctx).await,
        Cmd::Reset { yes } => run_reset(&url, yes, profile, ctx).await,
        Cmd::Prune {
            outbox_days,
            task_log_days,
        } => run_prune(&url, outbox_days, task_log_days, ctx).await,
        Cmd::RebuildProjection { name, all } => {
            run_rebuild_projection(&url, name.as_deref(), all, ctx).await
        }
    }
}

/// `kevin db rebuild-projection <name|--all>`: truncate the read model and
/// replay `core.events` from position 0 (`plan/10-observability-ops.md`
/// §Runbooks). Readers see stale rows until it finishes.
async fn run_rebuild_projection(
    url: &str,
    name: Option<&str>,
    all: bool,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let pool = Db::connect_url(url).await.map_err(|e| store_err(&e))?;
    let store: Arc<dyn EventStore> = Arc::new(PgEventStore::new(pool.clone()));
    let result = if all {
        projections::rebuild_all(pool.clone(), store).await
    } else {
        let name = name.unwrap_or_default();
        projections::rebuild(pool.clone(), store, name)
            .await
            .map(|report| vec![report])
    };
    pool.close().await;
    let reports = result.map_err(|e| projection_err(&e))?;
    if ctx.global.json {
        let rows: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "projection": r.name,
                    "events": r.events,
                    "position": r.position,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "rebuilt": rows }));
    } else if !ctx.global.quiet {
        for report in &reports {
            println!("{}", rebuild_summary(report));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn rebuild_summary(report: &RebuildReport) -> String {
    format!(
        "rebuilt {}: replayed {} events, checkpoint at position {}",
        report.name, report.events, report.position
    )
}

fn projection_err(err: &ProjectionError) -> anyhow::Error {
    let code = if matches!(err, ProjectionError::UnknownProjection { .. }) {
        exit::INVALID_ARGS
    } else {
        exit::FAILED
    };
    ExitError::new(code, err.to_string()).into()
}

async fn run_init(
    url: &str,
    admin_url: Option<String>,
    create_role: bool,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let (report, pool) = init(&InitOptions {
        target_url: url.to_owned(),
        admin_url,
        create_role,
    })
    .await
    .map_err(|e| store_err(&e))?;
    if !ctx.global.quiet {
        println!(
            "database {} (role {}): role {}, database {}, extension vector {}",
            report.database,
            report.role,
            created(report.role_created, create_role),
            created(report.database_created, true),
            created(report.extension_created, true),
        );
    }
    if !report.manual_steps.is_empty() {
        eprintln!("This connection lacks the privilege for some steps. Run as a superuser:");
        for step in &report.manual_steps {
            eprintln!("  {step}");
        }
    }
    let Some(pool) = pool else {
        return Err(ExitError::new(
            exit::FAILED,
            format!(
                "cannot connect to {}; complete the steps above, then run `kevin db init` again",
                redact_url(url)
            ),
        )
        .into());
    };
    let result = migrate::migrate(&pool, MigratePolicy::Apply).await;
    pool.close().await;
    let report = result.map_err(|e| store_err(&e))?;
    if !ctx.global.quiet {
        println!("{}", migrate_summary(&report));
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_migrate(url: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let pool = Db::connect_url(url).await.map_err(|e| store_err(&e))?;
    let result = migrate::migrate(&pool, MigratePolicy::Apply).await;
    pool.close().await;
    let report = result.map_err(|e| store_err(&e))?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "applied": report.applied,
                "already_applied": report.already_applied,
            })
        );
    } else if !ctx.global.quiet {
        println!("{}", migrate_summary(&report));
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_status(url: &str, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let pool = Db::connect_url(url).await.map_err(|e| store_err(&e))?;
    let result = migrate::status(&pool).await;
    pool.close().await;
    let status = result.map_err(|e| store_err(&e))?;
    if ctx.global.json {
        let entries: Vec<serde_json::Value> = status
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "version": e.version,
                    "description": e.description,
                    "state": state_name(e.state),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "database": redact_url(url),
                "current": status.is_current(),
                "pgvector_installed": status.pgvector_installed,
                "pending": status.pending(),
                "mismatched": status.mismatched(),
                "unknown_applied": status.unknown_applied,
                "migrations": entries,
            })
        );
    } else {
        println!("database: {}", redact_url(url));
        println!(
            "pgvector: {}",
            if status.pgvector_installed {
                "installed"
            } else {
                "MISSING"
            }
        );
        println!("migrations:");
        for e in &status.entries {
            println!(
                "  {:>4}  {:<32} {}",
                e.version,
                e.description,
                state_name(e.state)
            );
        }
        for v in &status.unknown_applied {
            println!("  {v:>4}  (unknown to this binary)      applied");
        }
        let pending = status.pending();
        if status.is_current() {
            println!("status: current ({} applied)", status.entries.len());
        } else {
            println!(
                "status: {} pending, {} mismatched, {} unknown",
                pending.len(),
                status.mismatched().len(),
                status.unknown_applied.len()
            );
        }
    }
    Ok(if status.is_current() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::FAILED)
    })
}

async fn run_reset(url: &str, yes: bool, profile: Profile, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if !yes {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            "`kevin db reset` destroys every Kevin table; pass --yes to confirm",
        )
        .into());
    }
    if profile != Profile::Laptop {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            format!("`kevin db reset` is only allowed with profile `laptop` (current: `{profile}`)"),
        )
        .into());
    }
    let pool = Db::connect_url(url).await.map_err(|e| store_err(&e))?;
    let result = migrate::reset(&pool).await;
    pool.close().await;
    let report = result.map_err(|e| store_err(&e))?;
    if !ctx.global.quiet {
        println!(
            "reset {}: dropped schemas {}; {}",
            redact_url(url),
            migrate::KEVIN_SCHEMAS.join(", "),
            migrate_summary(&report)
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_prune(
    url: &str,
    outbox_days: u32,
    task_log_days: u32,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let pool = Db::connect_url(url).await.map_err(|e| store_err(&e))?;
    let outbox = Outbox::new(pool.clone());
    let result = outbox
        .prune_delivered(Duration::from_secs(u64::from(outbox_days) * 86_400))
        .await;
    let logs = projections::TaskLog::new(pool.clone())
        .prune_older_than_days(task_log_days)
        .await;
    pool.close().await;
    let pruned = result.map_err(|e| store_err(&e))?;
    let log_lines = logs.map_err(|e| projection_err(&e))?;
    if !ctx.global.quiet {
        println!("pruned {pruned} delivered outbox rows older than {outbox_days} days");
        println!("pruned {log_lines} task log lines older than {task_log_days} days");
    }
    Ok(ExitCode::SUCCESS)
}


fn created(done: bool, attempted: bool) -> &'static str {
    match (done, attempted) {
        (true, _) => "created",
        (false, true) => "already present",
        (false, false) => "skipped",
    }
}

fn migrate_summary(report: &migrate::MigrationReport) -> String {
    if report.applied.is_empty() {
        format!(
            "migrations: nothing to apply ({} already applied)",
            report.already_applied.len()
        )
    } else {
        format!(
            "migrations: applied {:?} ({} were already applied)",
            report.applied,
            report.already_applied.len()
        )
    }
}

const fn state_name(state: MigrationState) -> &'static str {
    match state {
        MigrationState::Applied => "applied",
        MigrationState::Pending => "pending",
        MigrationState::ChecksumMismatch => "CHECKSUM MISMATCH",
    }
}

fn store_err(err: &StoreError) -> anyhow::Error {
    let code = if matches!(err, &StoreError::InvalidConfig(_)) {
        exit::INVALID_ARGS
    } else if err.is_unreachable() {
        exit::UNREACHABLE
    } else {
        exit::FAILED
    };
    ExitError::new(code, err.to_string()).into()
}

/// Resolves the database URL and profile from the configuration; `--url` wins.
fn resolve(flag: Option<&str>, ctx: &Ctx) -> anyhow::Result<(String, Profile)> {
    let resolved = load_from_ctx(ctx).map_err(|errors| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!("configuration: {errors}"),
        )
    })?;
    let profile = resolved.config.kevin.profile;
    if let Some(url) = flag {
        return Ok((url.to_owned(), profile));
    }
    let cfg = DatabaseCfg::from_config(&resolved.config.database).map_err(|e| store_err(&e))?;
    Ok((cfg.url, profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GlobalArgs;

    #[test]
    fn url_flag_wins_over_config() {
        let ctx = Ctx::new(GlobalArgs {
            set: vec!["database.url=postgres://b/y".into()],
            ..GlobalArgs::default()
        });
        let (url, profile) = resolve(Some("postgres://c/z"), &ctx).unwrap();
        assert_eq!(url, "postgres://c/z");
        assert_eq!(profile, Profile::Laptop);
        let (url, _) = resolve(None, &ctx).unwrap();
        assert_eq!(url, "postgres://b/y");
    }
}
