//! `kevin answer <question-id> …` — answer a question. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::QuestionId;

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "answer";

/// Arguments of `kevin answer`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Question id.
    pub question_id: QuestionId,
    /// Selected option label(s).
    #[arg(value_name = "OPTION")]
    pub options: Vec<String>,
    /// Free-text answer.
    #[arg(long, value_name = "TEXT")]
    pub text: Option<String>,
    /// Apply the question's default answer.
    #[arg(long, conflicts_with_all = ["options", "text"])]
    pub default: bool,
}

/// The `kevin answer` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Answer an open question")
}

/// Runs `kevin answer`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
