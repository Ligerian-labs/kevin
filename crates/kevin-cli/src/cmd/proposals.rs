//! `kevin proposals …` — prompt/config/routing proposals raised by evaluations. Owned by WS-19.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "proposals";

/// Arguments of `kevin proposals`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin proposals` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List proposals.
    Ls,
    /// Accept a proposal (routing proposals are applied; others are for humans).
    Accept {
        /// Proposal id.
        id: String,
    },
    /// Reject a proposal.
    Reject {
        /// Proposal id.
        id: String,
        /// Note recorded with the rejection.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
}

/// The `kevin proposals` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Review proposals raised by evaluations")
}

/// Runs `kevin proposals`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
