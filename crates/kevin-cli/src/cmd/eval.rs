//! `kevin eval …` — evaluations. Owned by WS-19.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "eval";

/// Arguments of `kevin eval`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin eval` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Re-run the judge on a terminal run.
    Rerun {
        /// Run id.
        run_id: RunId,
    },
}

/// The `kevin eval` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Evaluations: re-run the judge on a run")
}

/// Runs `kevin eval`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
