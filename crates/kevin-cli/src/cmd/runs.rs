//! `kevin runs …` — inspect and manage runs. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;
use kevin_domain::run::CancelRun;
use kevin_orchestrator::projections::{RunOverviewRow, RunQuery};
use kevin_orchestrator::services::CommandContext;
use kevin_store::EventStore;

use crate::cmd::answer::actor;
use crate::{Ctx, ExitError, embedded, exit, render};

/// Subcommand name.
pub const NAME: &str = "runs";

/// Events read per `kevin runs events` round trip.
const PAGE: usize = 256;

/// Arguments of `kevin runs`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin runs` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List runs.
    Ls {
        /// Filter by status.
        #[arg(long, value_name = "STATUS")]
        status: Option<String>,
        /// Maximum number of rows.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },
    /// Show one run (understanding, plan, tasks, questions, usage).
    Show {
        /// Run id.
        run_id: RunId,
    },
    /// Cancel a run.
    Cancel {
        /// Run id.
        run_id: RunId,
        /// Reason recorded on the `run.cancelled` event.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Print the run's event stream.
    Events {
        /// Run id.
        run_id: RunId,
        /// Start from this global position.
        #[arg(long, value_name = "N")]
        from: Option<u64>,
    },
    /// Follow a run live until it is terminal.
    Watch {
        /// Run id.
        run_id: RunId,
    },
}

/// The `kevin runs` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Inspect and manage runs")
}

/// Runs `kevin runs`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    // `watch` needs a live engine (it follows the bus); everything else is a
    // read model query or a single command.
    if let Cmd::Watch { run_id } = args.cmd {
        return crate::cmd::run::watch(run_id, ctx).await;
    }
    let backend = embedded::open_backend(ctx).await?;
    let result = dispatch(args.cmd, ctx, &backend).await;
    backend.close().await;
    result
}

async fn dispatch(
    cmd: Cmd,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    match cmd {
        Cmd::Ls { status, limit } => ls(status, limit, ctx, backend).await,
        Cmd::Show { run_id } => show(run_id, ctx, backend).await,
        Cmd::Cancel { run_id, reason } => cancel(run_id, reason, ctx, backend).await,
        Cmd::Events { run_id, from } => events(run_id, from, ctx, backend).await,
        Cmd::Watch { .. } => unreachable!("handled by run()"),
    }
}

async fn ls(
    status: Option<String>,
    limit: Option<u32>,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let query = RunQuery {
        status,
        cursor: None,
        limit: limit.map(|n| n as usize),
    };
    let page = backend.read_models().runs(&query).await?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "runs": page.items.iter().map(summary_json).collect::<Vec<_>>(),
            "next_cursor": page.next_cursor,
        }));
    } else if page.items.is_empty() {
        render::line("no runs yet");
    } else {
        let rows = page
            .items
            .iter()
            .map(|r| {
                vec![
                    render::short(r.run_id),
                    r.status.clone(),
                    r.goal_excerpt.clone(),
                    format!("{}/{}", r.tasks_succeeded, r.tasks_total),
                    render::money(r.cost_usd.get()),
                    render::age(r.updated_at),
                ]
            })
            .collect();
        render::line(&render::table(
            &["run", "status", "goal", "tasks", "usd", "age"],
            rows,
        ));
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(
    run_id: RunId,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let row = load(backend, run_id).await?;
    let tasks = backend.read_models().tasks_of_run(run_id.as_uuid()).await?;
    if ctx.global.json {
        let mut body = run_json(&row);
        body["tasks"] = serde_json::Value::Array(
            tasks.iter().map(crate::cmd::tasks::task_json).collect(),
        );
        render::json_line(&body);
        return Ok(ExitCode::SUCCESS);
    }

    render::line(&format!("run     {}", row.run_id));
    render::line(&format!("status  {}", row.status));
    render::line(&format!("mode    {}", row.mode));
    render::line(&format!("goal    {}", row.goal_text));
    render::line(&format!("cwd     {}", row.cwd));
    render::line(&format!(
        "usage   {} USD, {} in / {} out tokens, {}",
        render::money(row.cost_usd.get()),
        row.input_tokens,
        row.output_tokens,
        render::millis(row.wall_ms),
    ));
    if let Some(summary) = &row.summary {
        render::line(&format!("summary {summary}"));
    }
    if let Some(reason) = &row.failure_reason {
        render::line(&format!(
            "failure {reason}{}",
            row.failure_message
                .as_ref()
                .map_or_else(String::new, |m| format!(": {m}"))
        ));
    }
    if !row.open_question_ids.is_empty() {
        render::line(&format!(
            "open questions: {}",
            row.open_question_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !tasks.is_empty() {
        let rows = tasks
            .iter()
            .map(|t| {
                vec![
                    render::short(t.task_id),
                    t.kind.clone(),
                    t.status.clone(),
                    t.title.clone(),
                    t.route_model.clone().unwrap_or_else(|| "-".to_owned()),
                    render::money(t.cost_usd.get()),
                ]
            })
            .collect();
        render::line("");
        render::line(&render::table(
            &["task", "kind", "status", "title", "model", "usd"],
            rows,
        ));
    }
    Ok(ExitCode::SUCCESS)
}

async fn cancel(
    run_id: RunId,
    reason: Option<String>,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let by = actor();
    let reason = reason.unwrap_or_else(|| "cancelled from the CLI".to_owned());
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), run_id, by.clone());
    backend
        .run_service()
        .cancel(run_id, CancelRun { by, reason }, &cmd_ctx)
        .await
        .map_err(|e| ExitError::new(exit::FAILED, format!("cancel: {e}")))?;
    backend.catch_up().await?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({ "run_id": run_id, "cancelled": true }));
    } else {
        render::line(&format!("cancelled run {run_id}"));
    }
    Ok(ExitCode::SUCCESS)
}

async fn events(
    run_id: RunId,
    from: Option<u64>,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let mut position = from.unwrap_or(0);
    let mut printed = 0_u64;
    loop {
        let page = backend.store().read_all(position, PAGE).await?;
        if page.is_empty() {
            break;
        }
        position = page.last().map_or(position, |e| e.position);
        for stored in page {
            if stored.envelope.correlation_id != run_id.as_uuid() {
                continue;
            }
            printed += 1;
            if ctx.global.json {
                render::json_line(&crate::cmd::run::event_json(
                    stored.position,
                    &stored.envelope,
                ));
            } else {
                render::line(&crate::cmd::run::event_line(
                    stored.position,
                    &stored.envelope,
                ));
            }
        }
    }
    if printed == 0 && !ctx.global.json {
        render::line(&format!("no events for run {run_id}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Loads a run row or fails with `run_not_found`.
pub async fn load(
    backend: &embedded::Backend,
    run_id: RunId,
) -> anyhow::Result<RunOverviewRow> {
    backend
        .read_models()
        .run(run_id.as_uuid())
        .await?
        .ok_or_else(|| ExitError::new(exit::FAILED, format!("run {run_id} does not exist")).into())
}

/// The `RunSummaryDto`-shaped JSON of one overview row.
#[must_use]
pub fn summary_json(row: &RunOverviewRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.run_id,
        "status": row.status,
        "goal_excerpt": row.goal_excerpt,
        "usage": row.usage,
        "cost_usd": row.cost_usd.get().map(|d| d.to_string()),
        "task_counts": {
            "total": row.tasks_total,
            "succeeded": row.tasks_succeeded,
            "failed": row.tasks_failed,
            "cancelled": row.tasks_cancelled,
            "skipped": row.tasks_skipped,
        },
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

/// The `RunDto`-shaped JSON of one overview row.
#[must_use]
pub fn run_json(row: &RunOverviewRow) -> serde_json::Value {
    let mut body = summary_json(row);
    body["goal"] = serde_json::json!({
        "text": row.goal_text,
        "cwd": row.cwd,
        "repo_kind": row.repo_kind,
    });
    body["mode"] = serde_json::Value::String(row.mode.clone());
    body["budget"] = row.budget.clone();
    body["understanding"] = row.understanding.clone().unwrap_or(serde_json::Value::Null);
    body["plan"] = row.plan.clone().unwrap_or(serde_json::Value::Null);
    body["open_questions"] = serde_json::json!(row.open_question_ids);
    body["artifacts"] = row.artifacts.clone();
    body["summary"] = row
        .summary
        .clone()
        .map_or(serde_json::Value::Null, serde_json::Value::String);
    body["failure"] = row.failure_reason.clone().map_or(
        serde_json::Value::Null,
        |reason| {
            serde_json::json!({
                "reason": reason,
                "class": row.failure_class,
                "message": row.failure_message,
            })
        },
    );
    body["version"] = serde_json::json!(row.version);
    body
}
