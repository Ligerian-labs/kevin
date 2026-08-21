//! `kevin serve` — run the daemon (API + orchestrator). Owned by WS-20.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "serve";

/// Arguments of `kevin serve`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Kohral profile: expose the Kohral runtime contract (`kevin.profile = kohral`).
    #[arg(long)]
    pub kohral: bool,
    /// Address to bind the API on (overrides `server.bind`).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
}

/// The `kevin serve` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Run Kevin as a daemon (HTTP API + orchestrator)")
}

/// Runs `kevin serve`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
