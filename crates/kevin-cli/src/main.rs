//! `kevin` binary entry point. All logic lives in the `kevin_cli` library so
//! integration tests can build the command tree without spawning a process.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    kevin_cli::main_with_args(std::env::args_os()).await
}
