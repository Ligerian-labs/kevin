//! `kevin approve <run-id>` — approve a proposed plan. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;
use kevin_domain::run::ApprovePlan;
use kevin_orchestrator::services::CommandContext;

use crate::cmd::answer::actor;
use crate::{Ctx, ExitError, embedded, exit, render};

/// Subcommand name.
pub const NAME: &str = "approve";

/// Arguments of `kevin approve`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Run id.
    pub run_id: RunId,
}

/// The `kevin approve` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Approve a run's proposed plan")
}

/// Runs `kevin approve`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let backend = embedded::open_backend(ctx).await?;
    let by = actor();
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), args.run_id, by.clone());
    let result = backend
        .run_service()
        .approve_plan(args.run_id, ApprovePlan { by }, &cmd_ctx)
        .await;
    let catch_up = if result.is_ok() {
        backend.catch_up().await
    } else {
        Ok(())
    };
    backend.close().await;

    result.map_err(|e| ExitError::new(exit::FAILED, format!("approve: {e}")))?;
    catch_up?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({ "run_id": args.run_id, "approved": true }));
    } else {
        render::line(&format!("approved the plan of run {}", args.run_id));
    }
    Ok(ExitCode::SUCCESS)
}
