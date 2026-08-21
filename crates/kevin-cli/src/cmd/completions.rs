//! `kevin completions <shell>` — shell completion scripts (implemented in WS-00).

use std::process::ExitCode;

use clap::Args as _;

use crate::Ctx;

/// Subcommand name.
pub const NAME: &str = "completions";

/// Arguments of `kevin completions`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Shell to generate completions for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

/// The `kevin completions` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Print shell completions to stdout")
}

/// Runs `kevin completions`.
pub async fn run(args: Args, _ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let mut tree = crate::command();
    clap_complete::generate(args.shell, &mut tree, crate::BIN_NAME, &mut std::io::stdout());
    Ok(ExitCode::SUCCESS)
}
