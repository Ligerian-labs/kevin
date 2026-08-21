//! `kevin reject <run-id> --feedback` — reject a proposed plan. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "reject";

/// Arguments of `kevin reject`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Run id.
    pub run_id: RunId,
    /// Feedback for the planner's next proposal.
    #[arg(long, value_name = "TEXT")]
    pub feedback: String,
}

/// The `kevin reject` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Reject a run's proposed plan with feedback")
}

/// Runs `kevin reject`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
