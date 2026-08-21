//! `kevin tui` — open the terminal UI. Owned by WS-17.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "tui";

/// Arguments of `kevin tui`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Open directly on this run.
    #[arg(long, value_name = "RUN_ID")]
    pub run: Option<RunId>,
}

/// The `kevin tui` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Open the terminal UI")
}

/// Runs `kevin tui`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
