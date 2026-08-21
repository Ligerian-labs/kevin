//! `kevin routes …` — route leaderboard and routing explanations. Owned by WS-09.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::{ModelAlias, TaskKind};

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "routes";

/// Arguments of `kevin routes` (no subcommand = leaderboard).
#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    /// Only this task kind.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<TaskKind>,
    /// What to do (default: show the leaderboard).
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// `kevin routes` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Explain which route would be selected for a task kind.
    Explain {
        /// Task kind.
        #[arg(long, value_name = "KIND")]
        kind: TaskKind,
        /// Estimated complexity (low|medium|high).
        #[arg(long, value_name = "C")]
        complexity: Option<String>,
    },
    /// Reset the learned score of one (kind, alias) pair.
    Reset {
        /// Task kind.
        #[arg(long, value_name = "KIND")]
        kind: TaskKind,
        /// Model alias.
        #[arg(long, value_name = "ALIAS")]
        alias: ModelAlias,
    },
}

/// The `kevin routes` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Route leaderboard, explain and reset")
}

/// Runs `kevin routes`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
