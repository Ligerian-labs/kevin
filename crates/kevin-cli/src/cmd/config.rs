//! `kevin config …` — configuration files and token. Owned by WS-02.

use std::process::ExitCode;

use clap::Args as _;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "config";

/// Arguments of `kevin config`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin config` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Write the commented default config file and a fresh token.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print the effective config (secrets redacted).
    Show {
        /// Annotate each value with its source layer.
        #[arg(long)]
        sources: bool,
    },
    /// Validate the effective config; non-zero exit on errors.
    Validate,
    /// Replace the API token file.
    #[command(name = "rotate-token")]
    RotateToken,
}

/// The `kevin config` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Configuration: init, show, validate, rotate-token")
}

/// Runs `kevin config`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
