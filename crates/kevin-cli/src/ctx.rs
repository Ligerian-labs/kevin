//! Global flags, the per-invocation [`Ctx`] and exit-code conventions.

use std::path::PathBuf;

/// Process exit codes (`plan/07-api-and-tui.md` §3, `kevin run`).
pub mod exit {
    /// Run completed / command succeeded.
    pub const OK: u8 = 0;
    /// Run failed / generic command failure.
    pub const FAILED: u8 = 1;
    /// Run cancelled; also used by WS-00 stubs ("not implemented yet").
    pub const CANCELLED: u8 = 2;
    /// Stub commands exit with this code until their workstream lands.
    pub const NOT_IMPLEMENTED: u8 = 2;
    /// Invalid arguments or configuration.
    pub const INVALID_ARGS: u8 = 3;
    /// Server unreachable or unauthenticated.
    pub const UNREACHABLE: u8 = 4;
    /// Budget exhausted.
    pub const BUDGET_EXHAUSTED: u8 = 5;
    /// Interrupted by Ctrl-C.
    pub const INTERRUPTED: u8 = 130;
}

/// Flags accepted before (or after) any subcommand.
#[derive(Debug, Clone, Default, clap::Args)]
#[command(next_help_heading = "Global options")]
pub struct GlobalArgs {
    /// Extra config file, layered between the project file and the environment.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Override one config value (`section.key=value`); highest precedence, repeatable.
    #[arg(long = "set", global = true, value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub set: Vec<String>,

    /// Kevin server URL. When absent (and `client.server_url` is empty) Kevin
    /// runs embedded in this process.
    #[arg(long, global = true, value_name = "URL")]
    pub server: Option<String>,

    /// File holding the bearer token for `--server`.
    #[arg(long, global = true, value_name = "PATH")]
    pub token_file: Option<PathBuf>,

    /// Machine-readable JSON output (one object, or JSON lines for streams).
    #[arg(long, global = true)]
    pub json: bool,

    /// More log output (repeat for more).
    #[arg(short, long, global = true, action = clap::ArgAction::Count, conflicts_with = "quiet")]
    pub verbose: u8,

    /// Only print errors.
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

/// Everything a subcommand needs besides its own arguments. Later workstreams
/// add the resolved config, the API client / embedded runtime handle, etc.
#[derive(Debug, Clone)]
pub struct Ctx {
    /// Parsed global flags.
    pub global: GlobalArgs,
}

impl Ctx {
    /// Builds a context from parsed global flags.
    #[must_use]
    pub fn new(global: GlobalArgs) -> Self {
        Self { global }
    }
}

/// An error that carries the process exit code it should produce.
///
/// `anyhow` errors without an `ExitError` in their chain exit with
/// [`exit::FAILED`].
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExitError {
    /// Exit code for the process.
    pub code: u8,
    /// Human-readable message (printed to stderr).
    pub message: String,
}

impl ExitError {
    /// Creates an error with an explicit exit code.
    pub fn new(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Exit code for any error: the embedded one if present, else [`exit::FAILED`].
    #[must_use]
    pub fn code_of(err: &anyhow::Error) -> u8 {
        err.chain()
            .find_map(|e| e.downcast_ref::<ExitError>())
            .map_or(exit::FAILED, |e| e.code)
    }
}

/// The error every WS-00 stub returns (exit code [`exit::NOT_IMPLEMENTED`]).
#[must_use]
pub fn not_implemented(command: &str) -> anyhow::Error {
    ExitError::new(
        exit::NOT_IMPLEMENTED,
        format!("`kevin {command}` is not implemented yet (see plan/12-workstreams.md)"),
    )
    .into()
}
