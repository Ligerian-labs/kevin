//! `kevin approve <run-id>` — approve a proposed plan. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "approve";

/// Arguments of `kevin approve`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Run id.
    pub run_id: RunId,
}

/// The `kevin approve` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Approve a run's proposed plan")
}

/// Runs `kevin approve`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
