//! `kevin db …` — database lifecycle. Owned by WS-03.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "db";

/// Arguments of `kevin db`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin db` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Create role/database/extension `vector`, then migrate.
    Init {
        /// Also create the database role.
        #[arg(long)]
        create_role: bool,
        /// Admin connection URL used to create role/database.
        #[arg(long, value_name = "URL")]
        admin_url: Option<String>,
    },
    /// Apply pending migrations.
    Migrate,
    /// Show migration status.
    Status,
    /// Drop and recreate everything (laptop profile only).
    Reset {
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Prune old data (logs, delivered outbox rows, …).
    Prune,
    /// Rebuild a projection from the event store.
    #[command(name = "rebuild-projection")]
    RebuildProjection {
        /// Projection name.
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
    Args::augment_args(clap::Command::new(NAME)).about("Database lifecycle: init, migrate, status, reset")
}

/// Runs `kevin db`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
