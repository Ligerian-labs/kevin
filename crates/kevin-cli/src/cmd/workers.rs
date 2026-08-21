//! `kevin workers …` — worker adapters. Owned by WS-05.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "workers";

/// Arguments of `kevin workers`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin workers` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Check every enabled worker CLI (binary, version, auth); exit 1 if any is unhealthy.
    Doctor,
}

/// The `kevin workers` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Worker adapters (claude, codex, pi, opencode, fake)")
}

/// Runs `kevin workers`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
