//! `kevin kohral …` — Kohral runtime contract tooling. Owned by WS-22.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "kohral";

/// Conformance phases of `contract.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Phase {
    /// Basic contract.
    Basic,
    /// Accept a run, then the harness crashes the gateway.
    AcceptCrash,
    /// Verify ledger state after the crash.
    VerifyCrash,
}

/// Arguments of `kevin kohral`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin kohral` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Run the Kohral conformance suite against a Kevin gateway.
    Conformance {
        /// Gateway base URL.
        #[arg(long, value_name = "URL")]
        base_url: Option<String>,
        /// Kohral token.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Only this phase (default: all).
        #[arg(long, value_enum)]
        phase: Option<Phase>,
    },
}

/// The `kevin kohral` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Kohral runtime contract tooling")
}

/// Runs `kevin kohral`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
