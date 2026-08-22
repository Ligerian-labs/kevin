//! `kevin answer <question-id> …` — answer a question. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::question::AnswerQuestion;
use kevin_domain::{Answer, QuestionId, RunId};
use kevin_orchestrator::services::CommandContext;

use crate::cmd::questions::question_json;
use crate::{Ctx, ExitError, embedded, exit, render};

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
    let backend = embedded::open_backend(ctx).await?;
    let result = answer(&args, ctx, &backend).await;
    backend.close().await;
    result
}

async fn answer(
    args: &Args,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let question_id = args.question_id;
    let row = backend
        .read_models()
        .question(question_id.as_uuid())
        .await?
        .ok_or_else(|| {
            ExitError::new(
                exit::FAILED,
                format!("question {question_id} does not exist"),
            )
        })?;

    let answer = if args.default {
        let default = row.default_answer.clone().ok_or_else(|| {
            ExitError::new(
                exit::INVALID_ARGS,
                format!("question {question_id} has no default answer"),
            )
        })?;
        serde_json::from_value::<Answer>(default)?
    } else if args.options.is_empty() && args.text.is_none() {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            "give at least one option label, --text, or --default",
        )
        .into());
    } else {
        Answer {
            selected: args.options.clone(),
            free_text: args.text.clone(),
            answered_by: actor(),
        }
    };

    let run_id = RunId::from_uuid(row.run_id);
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), run_id, actor());
    backend
        .question_service()
        .answer(question_id, AnswerQuestion { answer }, &cmd_ctx)
        .await
        .map_err(|e| ExitError::new(exit::FAILED, format!("answer: {e}")))?;
    backend.catch_up().await?;

    let updated = backend.read_models().question(question_id.as_uuid()).await?;
    if ctx.global.json {
        let body = updated.as_ref().map_or_else(
            || serde_json::json!({ "id": question_id, "status": "answered" }),
            question_json,
        );
        render::json_line(&body);
    } else {
        render::line(&format!("answered {question_id}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Who the CLI acts as (`$USER`, else `cli`).
#[must_use]
pub fn actor() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "cli".to_owned())
}
