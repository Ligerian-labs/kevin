//! `kevin-cli` — the `kevin` binary (`plan/07-api-and-tui.md` §3).
//!
//! Interface crate that wires every other crate. Each subcommand lives in
//! `cmd/<name>.rs` and exposes `pub fn command() -> clap::Command` plus
//! `pub async fn run(args, ctx) -> anyhow::Result<ExitCode>`; `cmd/mod.rs`
//! registers them one line each so workstreams merge trivially. In WS-00 every
//! command except `completions` exits with [`ctx::exit::NOT_IMPLEMENTED`].
//!
//! Dependency direction: depends on everything; nothing depends on it.

use std::ffi::OsString;
use std::process::ExitCode;

use clap::{Args as _, FromArgMatches as _};

pub mod cmd;
pub mod ctx;
pub mod embedded;
pub mod observability;
pub mod render;
pub mod version;

pub use ctx::{Ctx, ExitError, GlobalArgs, exit, not_implemented};
pub use version::{BUILD_DATE, GIT_SHA, LONG_VERSION, VERSION};

/// Binary name.
pub const BIN_NAME: &str = "kevin";

/// The full `kevin` command tree (global flags + every subcommand).
#[must_use]
pub fn command() -> clap::Command {
    GlobalArgs::augment_args(clap::Command::new(BIN_NAME))
        .version(LONG_VERSION)
        .about("Kevin — autonomous agent runtime that orchestrates your coding agents")
        .long_about(
            "Kevin understands a goal, asks what is ambiguous, plans a task graph, routes each \
             task to the best (worker, model) pair, runs tasks in isolated workspaces, integrates \
             the results and evaluates itself. Without --server Kevin runs embedded in this \
             process; with it, the CLI is a thin API client.",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .propagate_version(true)
        .subcommands(cmd::commands())
}

/// Parses `args` (including the binary name), runs the selected subcommand and
/// maps errors to exit codes (`plan/07-api-and-tui.md` §3 and [`exit`]).
pub async fn main_with_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(err) => return clap_exit(&err),
    };
    let global = match GlobalArgs::from_arg_matches(&matches) {
        Ok(global) => global,
        Err(err) => return clap_exit(&err),
    };
    let Some((name, sub)) = matches.subcommand() else {
        // Unreachable: `subcommand_required(true)` makes clap report this.
        eprintln!("error: a subcommand is required");
        return ExitCode::from(exit::INVALID_ARGS);
    };
    let ctx = Ctx::new(global);
    match cmd::dispatch(name, sub, &ctx).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(ExitError::code_of(&err))
        }
    }
}

fn clap_exit(err: &clap::Error) -> ExitCode {
    // Help/version go to stdout with exit 0; usage errors go to stderr with exit 3.
    let _ = err.print();
    if matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::INVALID_ARGS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tree_is_well_formed() {
        command().debug_assert();
    }

    #[test]
    fn every_registered_subcommand_is_in_the_tree() {
        let tree = command();
        for name in cmd::NAMES {
            assert!(
                tree.find_subcommand(name).is_some(),
                "missing subcommand {name}"
            );
        }
    }
}
