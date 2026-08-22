//! `kevin config …` — configuration files and token. Owned by WS-02.
//!
//! - `init`: writes the commented default file (`plan/03` TOML) to the user
//!   config path and a fresh token; never overwrites without `--force`.
//! - `show`: effective config with secrets redacted (`--sources` annotates
//!   every key with its layer; `--json` emits `{config, sources}`).
//! - `validate`: exit 3 with every error when the config is invalid.
//! - `rotate-token`: replaces `server.auth_token_file` with a new token.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Args as _;
use kevin_config::{ConfigErrors, LoadOptions, Resolved, paths, token};

use crate::{Ctx, ExitError, exit};

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
    Args::augment_args(clap::Command::new(NAME))
        .about("Configuration: init, show, validate, rotate-token")
}

/// Runs `kevin config`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    match args.cmd {
        Cmd::Init { force } => init(force, ctx),
        Cmd::Show { sources } => show(sources, ctx),
        Cmd::Validate => validate(ctx),
        Cmd::RotateToken => rotate_token(ctx),
    }
}

/// Loads the effective configuration from the process environment plus the
/// global `--config` / `--set` flags.
pub fn load_from_ctx(ctx: &Ctx) -> Result<Resolved, ConfigErrors> {
    kevin_config::load(LoadOptions::from_process(
        ctx.global.config.clone(),
        ctx.global.set.clone(),
    ))
}

fn invalid(errors: &ConfigErrors, json: bool) -> anyhow::Error {
    if json {
        let list: Vec<serde_json::Value> = errors
            .iter()
            .map(|e| serde_json::json!({ "key": e.key(), "message": e.to_string() }))
            .collect();
        let body = serde_json::json!({ "ok": false, "errors": list });
        println!("{body}");
        ExitError::new(exit::INVALID_ARGS, format!("{} configuration error(s)", errors.len()))
            .into()
    } else {
        ExitError::new(exit::INVALID_ARGS, errors.to_string()).into()
    }
}

fn process_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// `server.auth_token_file` with `~` expanded — what the daemon reads and what
/// `rotate-token` writes.
#[must_use]
pub fn resolved_token_path(resolved: &Resolved) -> PathBuf {
    token_path(Some(resolved), &process_env())
}

fn token_path(resolved: Option<&Resolved>, env: &[(String, String)]) -> PathBuf {
    let configured = resolved.map_or_else(
        || kevin_config::Server::default().auth_token_file,
        |r| r.config.server.auth_token_file.clone(),
    );
    paths::expand_home(&configured, paths::home_dir(env).as_deref())
}

fn init(force: bool, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let env = process_env();
    let file = paths::user_config_file(&env);
    if file.exists() && !force {
        return Err(ExitError::new(
            exit::FAILED,
            format!("{} already exists (use --force to overwrite)", file.display()),
        )
        .into());
    }
    // Best effort: honour an already-configured token path; fall back to the default.
    let token_file = token_path(load_from_ctx(ctx).ok().as_ref(), &env);

    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, kevin_config::DEFAULT_TOML)?;
    let token_written = if token_file.exists() && !force {
        false
    } else {
        token::write_token_file(&token_file, &token::generate_token())?;
        true
    };

    if ctx.global.json {
        let body = serde_json::json!({
            "config_file": file,
            "token_file": token_file,
            "token_written": token_written,
        });
        println!("{body}");
    } else {
        println!("wrote {}", file.display());
        if token_written {
            println!("wrote token {}", token_file.display());
        } else {
            println!("kept existing token {}", token_file.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn show(sources: bool, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = load_from_ctx(ctx).map_err(|e| invalid(&e, ctx.global.json))?;
    if ctx.global.json {
        println!("{}", resolved.redacted_json());
    } else if sources {
        print!("{}", resolved.redacted_toml_with_sources());
    } else {
        print!("{}", resolved.redacted_toml());
    }
    Ok(ExitCode::SUCCESS)
}

fn validate(ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = load_from_ctx(ctx).map_err(|e| invalid(&e, ctx.global.json))?;
    let overrides = resolved
        .sources
        .values()
        .filter(|s| !matches!(s, kevin_config::Source::Default))
        .count();
    if ctx.global.json {
        let body = serde_json::json!({ "ok": true, "keys": resolved.sources.len(), "overrides": overrides });
        println!("{body}");
    } else {
        println!(
            "configuration is valid ({} keys, {overrides} overriding the defaults)",
            resolved.sources.len()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn rotate_token(ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = load_from_ctx(ctx).map_err(|e| invalid(&e, ctx.global.json))?;
    let path = token_path(Some(&resolved), &process_env());
    write_new_token(&path)?;
    if ctx.global.json {
        println!("{}", serde_json::json!({ "token_file": path }));
    } else {
        println!("wrote new token to {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn write_new_token(path: &Path) -> anyhow::Result<()> {
    token::write_token_file(path, &token::generate_token())
        .map_err(|e| anyhow::anyhow!("writing token {}: {e}", path.display()))
}
