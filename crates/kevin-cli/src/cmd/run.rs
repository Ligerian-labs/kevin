//! `kevin run <goal>` — start a run (embedded runtime or remote server). Owned by WS-12.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "run";

/// Arguments of `kevin run`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The goal to achieve (piped stdin is appended as extra context).
    #[arg(value_name = "GOAL")]
    pub goal: String,
    /// Target repository / working directory (default: current directory).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Allow a --cwd that is not inside a git/jj repository.
    #[arg(long, requires = "cwd")]
    pub allow_plain_dir: bool,
    /// Headless mode: auto-approve the plan, apply question defaults.
    #[arg(long)]
    pub headless: bool,
    /// Spend cap in USD (decimal, e.g. `5.00`).
    #[arg(long, value_name = "USD")]
    pub budget_usd: Option<String>,
    /// Wall-clock cap (e.g. `45m`, `2h`).
    #[arg(long, value_name = "DURATION")]
    pub budget_wall: Option<String>,
    /// Attach a file to the goal (repeatable).
    #[arg(long = "attach", value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<PathBuf>,
    /// Do not open the TUI; stream events as lines (or JSON lines with --json).
    #[arg(long)]
    pub no_tui: bool,
    /// With --no-tui, block until the run reaches a terminal state.
    #[arg(long)]
    pub wait: bool,
    /// Tag the run (repeatable).
    #[arg(long = "tag", value_name = "TAG", action = clap::ArgAction::Append)]
    pub tag: Vec<String>,
    /// In server mode, cancel the run when the CLI detaches (Ctrl-C).
    #[arg(long)]
    pub cancel_on_detach: bool,
}

/// The `kevin run` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Start a run from a goal")
}

/// Runs `kevin run`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
