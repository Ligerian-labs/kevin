//! `kevin tui` — open the terminal UI (`plan/07-api-and-tui.md` §3–4).
//!
//! The TUI is an API client and nothing else: it needs a daemon to talk to.
//! Server resolution follows `plan/07` §3: `--server`, then
//! `KEVIN__CLIENT__SERVER_URL`, then `client.server_url`. The bearer token comes
//! from `--token-file`, then `client.token_file`.
//!
//! When no server is configured, `plan/07` calls for an **embedded** runtime in
//! this process. That runtime is WS-20's (`kevin serve`), so until it lands this
//! command explains how to get one instead of failing obscurely.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args as _;
use kevin_config::{Resolved, paths, token};
use kevin_domain::RunId;

use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "tui";

/// What to print when neither `--server` nor `client.server_url` is set.
const NEEDS_A_SERVER: &str = "no Kevin daemon configured.\n\
     \n\
     The TUI is an API client: point it at a running daemon with\n\
     `kevin tui --server <url>`, or set `client.server_url` in your config.\n\
     \n\
     To use this machine, start one first:\n\
     \n\
    \x20   kevin serve            # in another terminal\n\
    \x20   kevin tui --server http://127.0.0.1:7777\n\
     \n\
     (An embedded, self-hosted TUI session is not available yet.)";

/// Arguments of `kevin tui`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Open directly on this run.
    #[arg(long, value_name = "RUN_ID")]
    pub run: Option<RunId>,
}

/// The `kevin tui` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Open the terminal UI")
}

/// Runs `kevin tui`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = crate::cmd::config::load_from_ctx(ctx)
        .map_err(|errors| ExitError::new(exit::INVALID_ARGS, errors.to_string()))?;

    let Some(server) = server_url(ctx, &resolved) else {
        return Err(ExitError::new(exit::INVALID_ARGS, NEEDS_A_SERVER).into());
    };
    let token = read_token(ctx, &resolved)?;

    let options = kevin_tui::Options::connect(&server, &token)
        .map_err(|e| ExitError::new(exit::INVALID_ARGS, e.to_string()))?
        .run(args.run);

    kevin_tui::run(options)
        .await
        .map_err(|e| ExitError::new(exit::UNREACHABLE, format!("{server}: {e}")))?;
    Ok(ExitCode::SUCCESS)
}

/// `--server` > `KEVIN__CLIENT__SERVER_URL` / `client.server_url`; `None` when
/// every layer left it empty.
fn server_url(ctx: &Ctx, resolved: &Resolved) -> Option<String> {
    ctx.global
        .server
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            let configured = resolved.config.client.server_url.trim();
            (!configured.is_empty()).then(|| configured.to_owned())
        })
}

/// Reads the bearer token file, with a message that says which path failed.
fn read_token(ctx: &Ctx, resolved: &Resolved) -> anyhow::Result<String> {
    let path = token_path(ctx, resolved);
    token::read_token_file(&path).map_err(|e| {
        ExitError::new(
            exit::UNREACHABLE,
            format!(
                "cannot read the API token from {}: {e}\n\
                 run `kevin config init` on the daemon and copy the token, or pass --token-file",
                path.display()
            ),
        )
        .into()
    })
}

fn token_path(ctx: &Ctx, resolved: &Resolved) -> PathBuf {
    let configured = ctx
        .global
        .token_file
        .clone()
        .unwrap_or_else(|| resolved.config.client.token_file.clone());
    let env: Vec<(String, String)> = std::env::vars().collect();
    paths::expand_home(&configured, paths::home_dir(&env).as_deref())
}
