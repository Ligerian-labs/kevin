//! `kevin tasks …` — inspect and manage tasks. Owned by WS-12.

use std::process::ExitCode;
use std::time::Duration;

use clap::Args as _;
use kevin_domain::task::{CancelTask, RetryTask};
use kevin_domain::{RunId, TaskId};
use kevin_orchestrator::projections::{TaskBoardRow, TaskLogQuery};
use kevin_orchestrator::services::CommandContext;

use crate::cmd::answer::actor;
use crate::{Ctx, ExitError, embedded, exit, render};

/// Subcommand name.
pub const NAME: &str = "tasks";

/// Poll interval of `kevin tasks log --follow`.
const FOLLOW_POLL: Duration = Duration::from_millis(300);

/// Arguments of `kevin tasks`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin tasks` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List the tasks of a run.
    Ls {
        /// Run id.
        run_id: RunId,
    },
    /// Show one task (attempts, route, usage, artifacts).
    Show {
        /// Task id.
        task_id: TaskId,
    },
    /// Print a task's worker log.
    Log {
        /// Task id.
        task_id: TaskId,
        /// Keep streaming new lines.
        #[arg(long)]
        follow: bool,
        /// Only this attempt number.
        #[arg(long, value_name = "N")]
        attempt: Option<u8>,
    },
    /// Retry a failed task.
    Retry {
        /// Task id.
        task_id: TaskId,
    },
    /// Cancel a task.
    Cancel {
        /// Task id.
        task_id: TaskId,
    },
}

/// The `kevin tasks` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Inspect and manage tasks")
}

/// Runs `kevin tasks`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
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
        Cmd::Ls { run_id } => ls(run_id, ctx, backend).await,
        Cmd::Show { task_id } => show(task_id, ctx, backend).await,
        Cmd::Log {
            task_id,
            follow,
            attempt,
        } => log(task_id, follow, attempt, ctx, backend).await,
        Cmd::Retry { task_id } => {
            mutate(task_id, ctx, backend, Mutation::Retry).await
        }
        Cmd::Cancel { task_id } => {
            mutate(task_id, ctx, backend, Mutation::Cancel).await
        }
    }
}

async fn ls(
    run_id: RunId,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let tasks = backend.read_models().tasks_of_run(run_id.as_uuid()).await?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "run_id": run_id,
            "tasks": tasks.iter().map(task_json).collect::<Vec<_>>(),
        }));
    } else if tasks.is_empty() {
        render::line(&format!("run {run_id} has no tasks yet"));
    } else {
        let rows = tasks
            .iter()
            .map(|t| {
                vec![
                    render::short(t.task_id),
                    t.kind.clone(),
                    t.status.clone(),
                    t.title.clone(),
                    t.route_model.clone().unwrap_or_else(|| "-".to_owned()),
                    t.attempt_count.to_string(),
                    render::money(t.cost_usd.get()),
                ]
            })
            .collect();
        render::line(&render::table(
            &["task", "kind", "status", "title", "model", "att", "usd"],
            rows,
        ));
    }
    Ok(ExitCode::SUCCESS)
}

async fn show(
    task_id: TaskId,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let task = load(backend, task_id).await?;
    if ctx.global.json {
        render::json_line(&task_json(&task));
        return Ok(ExitCode::SUCCESS);
    }
    render::line(&format!("task    {}", task.task_id));
    render::line(&format!("run     {}", task.run_id));
    render::line(&format!("kind    {}", task.kind));
    render::line(&format!("status  {}", task.status));
    render::line(&format!("title   {}", task.title));
    render::line(&format!(
        "route   {}",
        task.route_model.clone().unwrap_or_else(|| "-".to_owned())
    ));
    render::line(&format!(
        "usage   {} USD, {} in / {} out tokens, {}",
        render::money(task.cost_usd.get()),
        task.input_tokens,
        task.output_tokens,
        render::millis(task.wall_ms),
    ));
    if let Some(summary) = &task.summary {
        render::line(&format!("summary {summary}"));
    }
    if let Some(message) = &task.failure_message {
        render::line(&format!(
            "failure {}{message}",
            task.failure_class
                .as_ref()
                .map_or_else(String::new, |c| format!("{c}: "))
        ));
    }
    Ok(ExitCode::SUCCESS)
}

async fn log(
    task_id: TaskId,
    follow: bool,
    attempt: Option<u8>,
    ctx: &Ctx,
    backend: &embedded::Backend,
) -> anyhow::Result<ExitCode> {
    let mut after_seq = 0_u64;
    loop {
        let query = TaskLogQuery {
            task_id: task_id.as_uuid(),
            attempt: attempt.map(i32::from),
            after_seq: (after_seq > 0).then_some(after_seq),
            limit: None,
        };
        let page = backend.read_models().task_log(&query).await?;
        for row in &page.items {
            after_seq = after_seq.max(u64::try_from(row.seq).unwrap_or(after_seq));
            if ctx.global.json {
                render::json_line(&serde_json::json!({
                    "seq": row.seq,
                    "attempt": row.attempt,
                    "at": row.at,
                    "kind": row.kind,
                    "payload": row.payload,
                }));
            } else {
                render::line(&format!(
                    "[{}] #{} {:<12} {}",
                    render::clock(row.at),
                    row.attempt,
                    row.kind,
                    compact(&row.payload)
                ));
            }
        }
        if !follow {
            break;
        }
        let task = load(backend, task_id).await?;
        if page.items.is_empty() && is_terminal(&task.status) {
            break;
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
    Ok(ExitCode::SUCCESS)
}

enum Mutation {
    Retry,
    Cancel,
}

async fn mutate(
    task_id: TaskId,
    ctx: &Ctx,
    backend: &embedded::Backend,
    what: Mutation,
) -> anyhow::Result<ExitCode> {
    let task = load(backend, task_id).await?;
    let run_id = RunId::from_uuid(task.run_id);
    let by = actor();
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), run_id, by);
    let (verb, result) = match what {
        Mutation::Retry => (
            "retried",
            backend
                .task_service()
                .retry_task(
                    task_id,
                    RetryTask {
                        reason: "retried from the CLI".to_owned(),
                    },
                    &cmd_ctx,
                )
                .await,
        ),
        Mutation::Cancel => (
            "cancelled",
            backend
                .task_service()
                .cancel_task(
                    task_id,
                    CancelTask {
                        reason: "cancelled from the CLI".to_owned(),
                    },
                    &cmd_ctx,
                )
                .await,
        ),
    };
    result.map_err(|e| ExitError::new(exit::FAILED, format!("{verb} task: {e}")))?;
    backend.catch_up().await?;
    if ctx.global.json {
        render::json_line(&serde_json::json!({ "task_id": task_id, "action": verb }));
    } else {
        render::line(&format!("{verb} task {task_id}"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Loads a task row or fails with `task_not_found`.
pub async fn load(
    backend: &embedded::Backend,
    task_id: TaskId,
) -> anyhow::Result<TaskBoardRow> {
    backend
        .read_models()
        .task(task_id.as_uuid())
        .await?
        .ok_or_else(|| {
            ExitError::new(exit::FAILED, format!("task {task_id} does not exist")).into()
        })
}

/// The `TaskDto`-shaped JSON of one task-board row.
#[must_use]
pub fn task_json(row: &TaskBoardRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.task_id,
        "run_id": row.run_id,
        "kind": row.kind,
        "title": row.title,
        "status": row.status,
        "route": row.route,
        "attempts": row.attempts,
        "depends_on": row.depends_on,
        "usage": row.usage,
        "cost_usd": row.cost_usd.get().map(|d| d.to_string()),
        "artifacts": row.artifacts,
        "acceptance_criteria": row.acceptance_criteria,
        "summary": row.summary,
        "failure": row.failure_message.as_ref().map(|message| serde_json::json!({
            "class": row.failure_class,
            "message": message,
        })),
    })
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "skipped")
}

fn compact(payload: &serde_json::Value) -> String {
    payload
        .as_str()
        .map_or_else(|| payload.to_string(), ToOwned::to_owned)
}
