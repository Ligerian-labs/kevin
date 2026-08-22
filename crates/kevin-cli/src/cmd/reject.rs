//! `kevin reject <run-id> --feedback` — reject a proposed plan. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;
use kevin_domain::run::RejectPlan;
use kevin_orchestrator::services::CommandContext;

use crate::cmd::answer::actor;
use crate::{Ctx, ExitError, embedded, exit, render};

/// Subcommand name.
pub const NAME: &str = "reject";

/// Arguments of `kevin reject`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Run id.
    pub run_id: RunId,
    /// Feedback for the planner's next proposal.
    #[arg(long, value_name = "TEXT")]
    pub feedback: String,
}

/// The `kevin reject` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Reject a run's proposed plan with feedback")
}

/// Runs `kevin reject`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.feedback.trim().is_empty() {
        return Err(ExitError::new(exit::INVALID_ARGS, "--feedback must not be empty").into());
    }
    let backend = embedded::open_backend(ctx).await?;
    let by = actor();
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), args.run_id, by.clone());
    let result = backend
        .run_service()
        .reject_plan(
            args.run_id,
            RejectPlan {
                by,
                feedback: args.feedback.clone(),
            },
            &cmd_ctx,
        )
        .await;
    let catch_up = if result.is_ok() {
        backend.catch_up().await
    } else {
        Ok(())
    };
    backend.close().await;

    result.map_err(|e| ExitError::new(exit::FAILED, format!("reject: {e}")))?;
    catch_up?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "run_id": args.run_id,
            "rejected": true,
            "feedback": args.feedback,
        }));
    } else {
        render::line(&format!("rejected the plan of run {}", args.run_id));
    }
    Ok(ExitCode::SUCCESS)
}
