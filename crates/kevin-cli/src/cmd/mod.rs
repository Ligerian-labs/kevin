//! Subcommand registry. One line per subcommand inside `subcommands!` — add
//! yours in the position given by `plan/07-api-and-tui.md` and nothing else
//! changes here.
//!
//! Each `cmd/<name>.rs` exposes:
//! - `pub const NAME: &str`
//! - `pub struct Args` (`#[derive(clap::Args)]`)
//! - `pub fn command() -> clap::Command`
//! - `pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode>`

use std::process::ExitCode;

use clap::FromArgMatches as _;

use crate::Ctx;

macro_rules! subcommands {
    ($($module:ident),* $(,)?) => {
        $(
            // Stubs await nothing yet; the signature is frozen as `async`.
            #[allow(clippy::unused_async)]
            pub mod $module;
        )*

        /// Names of every registered subcommand, in help order.
        pub const NAMES: &[&str] = &[$($module::NAME),*];

        /// Every subcommand's `clap::Command`, in help order.
        #[must_use]
        pub fn commands() -> Vec<clap::Command> {
            vec![$($module::command()),*]
        }

        /// Parses the matched subcommand's arguments and runs it.
        pub async fn dispatch(
            name: &str,
            matches: &clap::ArgMatches,
            ctx: &Ctx,
        ) -> anyhow::Result<ExitCode> {
            match name {
                $($module::NAME => $module::run($module::Args::from_arg_matches(matches)?, ctx).await,)*
                other => anyhow::bail!("unknown command `{other}`"),
            }
        }
    };
}

subcommands! {
    run,
    serve,
    tui,
    runs,
    tasks,
    questions,
    answer,
    approve,
    reject,
    db,
    config,
    workers,
    routes,
    lessons,
    memory,
    eval,
    proposals,
    cost,
    kohral,
    completions,
}
