//! `kevin tasks …` — inspect and manage tasks. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::{RunId, TaskId};

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "tasks";

/// Arguments of `kevin tasks`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin tasks` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List the tasks of a run.
    Ls {
        /// Run id.
        run_id: RunId,
    },
    /// Show one task (attempts, route, usage, artifacts).
    Show {
        /// Task id.
        task_id: TaskId,
    },
    /// Print a task's worker log.
    Log {
        /// Task id.
        task_id: TaskId,
        /// Keep streaming new lines.
        #[arg(long)]
        follow: bool,
        /// Only this attempt number.
        #[arg(long, value_name = "N")]
        attempt: Option<u8>,
    },
    /// Retry a failed task.
    Retry {
        /// Task id.
        task_id: TaskId,
    },
    /// Cancel a task.
    Cancel {
        /// Task id.
        task_id: TaskId,
    },
}

/// The `kevin tasks` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Inspect and manage tasks")
}

/// Runs `kevin tasks`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
