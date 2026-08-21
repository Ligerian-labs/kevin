//! `kevin cost` — spend report. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "cost";

/// Grouping dimension of the cost report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    /// Per run.
    Run,
    /// Per model alias.
    Model,
    /// Per task kind.
    Kind,
}

/// Arguments of `kevin cost`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Only spend since this long ago (e.g. `7d`, `24h`).
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,
    /// Group rows by run, model or kind.
    #[arg(long, value_enum, value_name = "DIM")]
    pub group_by: Option<GroupBy>,
    /// Only this run.
    #[arg(long, value_name = "RUN_ID")]
    pub run: Option<RunId>,
}

/// The `kevin cost` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Spend report (USD, tokens) from the cost ledger")
}

/// Runs `kevin cost`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
