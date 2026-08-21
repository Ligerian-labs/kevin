//! `kevin lessons` — learned lessons from memory. Owned by WS-18.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "lessons";

/// Arguments of `kevin lessons`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Maximum number of lessons.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// Only lessons scoped to the current repository.
    #[arg(long)]
    pub repo: bool,
}

/// The `kevin lessons` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("List lessons learned from evaluations")
}

/// Runs `kevin lessons`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
