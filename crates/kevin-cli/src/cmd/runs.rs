//! `kevin runs …` — inspect and manage runs. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "runs";

/// Arguments of `kevin runs`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin runs` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List runs.
    Ls {
        /// Filter by status.
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
        /// Maximum number of rows.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },
    /// Show one run (understanding, plan, tasks, questions, usage).
    Show {
        /// Run id.
        run_id: RunId,
    },
    /// Cancel a run.
    Cancel {
        /// Run id.
        run_id: RunId,
        /// Reason recorded on the `run.cancelled` event.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Print the run's event stream.
    Events {
        /// Run id.
        run_id: RunId,
        /// Start from this global position.
        #[arg(long, value_name = "N")]
        from: Option<u64>,
    },
    /// Follow a run live until it is terminal.
    Watch {
        /// Run id.
        run_id: RunId,
    },
}

/// The `kevin runs` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Inspect and manage runs")
}

/// Runs `kevin runs`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
