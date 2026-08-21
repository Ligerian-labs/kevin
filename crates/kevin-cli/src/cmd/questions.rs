//! `kevin questions …` — the question inbox. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "questions";

/// Arguments of `kevin questions`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin questions` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List open questions.
    Ls {
        /// Only questions of this run.
        #[arg(long, value_name = "RUN_ID")]
        run: Option<RunId>,
    },
}

/// The `kevin questions` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("List open questions")
}

/// Runs `kevin questions`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
