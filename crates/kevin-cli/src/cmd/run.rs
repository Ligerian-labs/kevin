//! `kevin run <goal>` — start a run and follow it (`plan/07-api-and-tui.md` §3,
//! `plan/05-orchestration.md` §3). Owned by WS-12.
//!
//! Without `--server` the CLI boots an [`EmbeddedRuntime`](crate::embedded::EmbeddedRuntime)
//! in this process, submits `StartRun`, and follows the run on the event bus
//! until it is terminal.
//!
//! # Streaming protocol
//!
//! Human mode prints one line per event:
//!
//! ```text
//! [12:00:01] run.started              add a /healthz endpoint
//! [12:00:04] task.attempt_started     implement: add the route (sonnet5-claude #1)
//! ```
//!
//! `--json` prints **one JSON object per line**, all of them objects with a
//! `type` discriminator:
//!
//! | `type` | When | Fields |
//! |---|---|---|
//! | `run_started` | once, before the stream | `run_id`, `goal`, `cwd`, `mode` |
//! | `event` | one per domain event of the run | `position`, `event_id`, `event_type`, `occurred_at`, `aggregate_type`, `aggregate_id`, `aggregate_version`, `run_id`, `actor`, `payload` |
//! | `question` | a question needs a human and the CLI cannot prompt | `question_id`, `text`, `options`, `hint` |
//! | `plan` | a plan needs approval and the CLI cannot prompt | `run_id`, `tasks`, `hint` |
//! | `summary` | once, after the terminal event | `run_id`, `status`, `summary`, `usage`, `cost_usd`, `artifacts`, `exit_code` |
//!
//! # Exit codes (`plan/07-api-and-tui.md` §3)
//!
//! `0` completed, `1` failed, `2` cancelled (by someone else), `3` invalid
//! arguments/config, `4` database/server unreachable, `5` budget exhausted,
//! `130` Ctrl-C — which **cancels** the embedded run (`run.cancelled`) before
//! exiting.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use clap::Args as _;
use kevin_bus::{EventBus as _, SubscriptionFilter};
use kevin_config::{KevinConfig, RoutingPolicy};
use kevin_domain::run::StartRun;
use kevin_domain::{
    ArtifactId, ArtifactKind, ArtifactRef, Budget, Goal, IdGen as _, ModelAlias, RepoKind, RunId,
    RunMode,
};
use kevin_orchestrator::services::CommandContext;
use rust_decimal::Decimal;

use crate::cmd::answer::actor;
use crate::embedded::{Backend, EmbeddedRuntime};
use crate::{Ctx, ExitError, embedded, exit, render};

mod follow;
mod prompt;

pub use follow::{Follow, Outcome, event_json, event_line};

/// Subcommand name.
pub const NAME: &str = "run";

/// How long the CLI waits for the projections to catch up with the terminal
/// event before printing the summary.
pub const PROJECTION_GRACE: Duration = Duration::from_secs(10);

/// Arguments of `kevin run`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// The goal to achieve (piped stdin is appended as extra context in --headless mode).
    #[arg(value_name = "GOAL")]
    pub goal: String,
    /// Target repository / working directory (default: current directory).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Allow a --cwd that is not inside a git/jj repository.
    #[arg(long)]
    pub allow_plain_dir: bool,
    /// Headless mode: auto-approve the plan, apply question defaults.
    #[arg(long)]
    pub headless: bool,
    /// Spend cap in USD (decimal, e.g. `5.00`).
    #[arg(long, value_name = "USD")]
    pub budget_usd: Option<String>,
    /// Wall-clock cap (e.g. `45m`, `2h`).
    #[arg(long, value_name = "DURATION")]
    pub budget_wall: Option<String>,
    /// Cap on concurrently running task attempts.
    #[arg(long, value_name = "N")]
    pub max_parallel: Option<u16>,
    /// Pin every routed task to this model alias.
    #[arg(long, value_name = "ALIAS")]
    pub model: Option<ModelAlias>,
    /// Attach a file to the goal (repeatable).
    #[arg(long = "attach", value_name = "FILE", action = clap::ArgAction::Append)]
    pub attach: Vec<PathBuf>,
    /// Do not open the TUI; stream events as lines (or JSON lines with --json).
    #[arg(long)]
    pub no_tui: bool,
    /// With --no-tui, block until the run reaches a terminal state.
    #[arg(long)]
    pub wait: bool,
    /// Tag the run (repeatable).
    #[arg(long = "tag", value_name = "TAG", action = clap::ArgAction::Append)]
    pub tag: Vec<String>,
    /// Approve the proposed plan without asking.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// In server mode, cancel the run when the CLI detaches (Ctrl-C).
    #[arg(long)]
    pub cancel_on_detach: bool,
}

/// The `kevin run` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Start a run from a goal")
}

/// Runs `kevin run`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let mut config = embedded::resolve_embedded(ctx)?;
    install_logging(&config, ctx);
    let cwd = resolve_cwd(args.cwd.as_deref(), args.allow_plain_dir)?;
    let repo_kind = detect_repo(&cwd);
    let budget = budget(&config, &args)?;
    if let Some(alias) = &args.model {
        pin_model(&mut config, alias)?;
    }
    if let Some(parallel) = args.max_parallel {
        config.budget.max_parallel_tasks = parallel.max(1);
    }
    let auto_approve = args.yes || args.headless || config.kevin.auto_approve_plans;
    config.kevin.auto_approve_plans = auto_approve;

    if !args.no_tui && !ctx.global.json && render::stdout_is_tty() {
        // TODO(ws-20): once `kevin serve` can bind the embedded API, hand this
        // run to `kevin tui --run`. WS-17's TUI is an API client, so until a
        // daemon exists `kevin run` always streams.
        eprintln!(
            "note: the TUI needs a daemon (`kevin serve`, WS-20); streaming this run instead"
        );
    }

    let goal_text = goal_text(&args)?;
    let goal = Goal {
        text: goal_text,
        attachments: attachments(&args.attach)?,
        cwd: cwd.clone(),
        repo_kind,
    };

    let config = Arc::new(config);
    let runtime = EmbeddedRuntime::start_in(Arc::clone(&config), &cwd).await?;
    let outcome = start_and_follow(&runtime, &args, ctx, goal, budget, auto_approve).await;
    runtime.shutdown().await;
    outcome
}

async fn start_and_follow(
    runtime: &EmbeddedRuntime,
    args: &Args,
    ctx: &Ctx,
    goal: Goal,
    budget: Budget,
    auto_approve: bool,
) -> anyhow::Result<ExitCode> {
    let backend = runtime.backend();
    let run_id = backend.ids().run_id();
    let mode = if args.headless {
        RunMode::Headless
    } else {
        RunMode::Interactive
    };

    // Subscribe *before* the command so no event of this run can be missed.
    let stream = backend
        .bus_erased()
        .subscribe(SubscriptionFilter::for_run(run_id.as_uuid()).named("kevin-run"));

    let by = actor();
    let cmd_ctx = CommandContext::user(backend.ids().as_ref(), run_id, by.clone());
    runtime
        .handle()
        .start_run(
            StartRun {
                run_id,
                goal: goal.clone(),
                mode: mode.clone(),
                budget,
                requested_by: by,
                auto_approve_plans: auto_approve,
            },
            &cmd_ctx,
        )
        .await
        .map_err(|e| ExitError::new(exit::FAILED, format!("start run: {e}")))?;

    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "type": "run_started",
            "run_id": run_id,
            "goal": goal.text,
            "cwd": goal.cwd,
            "mode": mode,
        }));
    } else if !ctx.global.quiet {
        render::line(&format!("run {run_id} started"));
    }

    let mut follow = Follow::new(runtime, run_id, ctx, auto_approve, args.headless);
    let outcome = follow.follow(stream).await?;
    summarise(backend, run_id, &outcome, ctx).await?;
    Ok(ExitCode::from(outcome.exit_code()))
}

/// Follows an existing run from the store (`kevin runs watch`): no engine is
/// booted, so a run driven by another process is observed, not resumed.
pub async fn watch(run_id: RunId, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let backend = embedded::open_backend(ctx).await?;
    let outcome = follow::watch_store(&backend, run_id, ctx).await;
    let summarised = match &outcome {
        Ok(outcome) => summarise(&backend, run_id, outcome, ctx).await,
        Err(_) => Ok(()),
    };
    backend.close().await;
    summarised?;
    Ok(ExitCode::from(outcome?.exit_code()))
}

/// Prints the final summary (human) or the `summary` JSON line.
async fn summarise(
    backend: &Backend,
    run_id: RunId,
    outcome: &Outcome,
    ctx: &Ctx,
) -> anyhow::Result<()> {
    let row = follow::await_projection(backend, run_id, PROJECTION_GRACE).await?;
    let artifacts = backend
        .read_models()
        .artifacts_of_run(run_id.as_uuid())
        .await
        .unwrap_or_default();

    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "type": "summary",
            "run_id": run_id,
            "status": row.as_ref().map_or_else(|| outcome.status().to_owned(), |r| r.status.clone()),
            "summary": row.as_ref().and_then(|r| r.summary.clone()),
            "usage": row.as_ref().map(|r| r.usage.clone()),
            "cost_usd": row.as_ref().and_then(|r| r.cost_usd.get()).map(|d| d.to_string()),
            "artifacts": artifacts.iter().map(|a| serde_json::json!({
                "id": a.artifact_id, "kind": a.kind, "uri": a.uri,
            })).collect::<Vec<_>>(),
            "exit_code": outcome.exit_code(),
        }));
        return Ok(());
    }
    if ctx.global.quiet {
        return Ok(());
    }

    let Some(row) = row else {
        render::line(&format!("{} (no read model yet)", outcome.status()));
        return Ok(());
    };
    render::line(&format!(
        "\n{} — {} USD, {} in / {} out tokens, {}",
        outcome.headline(),
        render::money(row.cost_usd.get()),
        row.input_tokens,
        row.output_tokens,
        render::millis(row.wall_ms),
    ));
    if let Some(summary) = &row.summary {
        render::line(&format!("summary: {summary}"));
    }
    if let Some(message) = &row.failure_message {
        render::line(&format!("failure: {message}"));
    }
    if !artifacts.is_empty() {
        render::line("artifacts:");
        for artifact in &artifacts {
            render::line(&format!("  - {} {}", artifact.kind, artifact.uri));
        }
    }
    Ok(())
}

/// Sends the runtime's own logs to **stderr** (stdout carries the event
/// stream), at a level driven by `-v`/`-q` over `telemetry.log_level`.
///
/// `kevin_telemetry::init` writes to stdout and installs a metrics recorder,
/// which is `kevin serve`'s job (WS-20), not a foreground run's.
fn install_logging(config: &KevinConfig, ctx: &Ctx) {
    let mut telemetry = kevin_telemetry::TelemetryConfig::from(&config.telemetry);
    telemetry.log_level = match (ctx.global.quiet, ctx.global.verbose) {
        (true, _) => "error".to_owned(),
        (_, 0) => "warn".to_owned(),
        (_, 1) => "info".to_owned(),
        (_, 2) => "debug".to_owned(),
        _ => "trace".to_owned(),
    };
    if let Ok(subscriber) = kevin_telemetry::build_subscriber(&telemetry, std::io::stderr) {
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
}

// ---------------------------------------------------------------------------
// Argument resolution
// ---------------------------------------------------------------------------

/// Canonicalises the working directory and enforces the repository rule.
fn resolve_cwd(cwd: Option<&Path>, allow_plain_dir: bool) -> anyhow::Result<PathBuf> {
    let raw = match cwd {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("current directory: {e}")))?,
    };
    let resolved = raw.canonicalize().map_err(|e| {
        ExitError::new(exit::INVALID_ARGS, format!("{}: {e}", raw.display()))
    })?;
    if !resolved.is_dir() {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            format!("{} is not a directory", resolved.display()),
        )
        .into());
    }
    if detect_repo(&resolved) == RepoKind::None && !allow_plain_dir {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            format!(
                "{} is not inside a git or jj repository (pass --allow-plain-dir to run anyway)",
                resolved.display()
            ),
        )
        .into());
    }
    Ok(resolved)
}

/// `.jj` wins over `.git` when both are present (`plan/05` §3.1).
fn detect_repo(dir: &Path) -> RepoKind {
    for candidate in dir.ancestors() {
        if candidate.join(".jj").exists() {
            return RepoKind::Jj;
        }
        if candidate.join(".git").exists() {
            return RepoKind::Git;
        }
    }
    RepoKind::None
}

/// Config defaults merged with the command's overrides (`plan/05` §3.1).
fn budget(config: &KevinConfig, args: &Args) -> anyhow::Result<Budget> {
    let mut budget = Budget::unlimited()
        .with_max_usd(config.budget.default_run_usd)
        .with_max_wall(config.budget.default_run_wall)
        .with_max_attempts(config.budget.max_attempts)
        .with_max_parallel(config.budget.max_parallel_tasks);
    if let Some(raw) = &args.budget_usd {
        budget.max_usd = Some(Decimal::from_str(raw).map_err(|e| {
            ExitError::new(exit::INVALID_ARGS, format!("--budget-usd {raw}: {e}"))
        })?);
    }
    if let Some(raw) = &args.budget_wall {
        budget.max_wall = Some(humantime::parse_duration(raw).map_err(|e| {
            ExitError::new(exit::INVALID_ARGS, format!("--budget-wall {raw}: {e}"))
        })?);
    }
    if let Some(parallel) = args.max_parallel {
        budget.max_parallel = parallel.max(1);
    }
    Ok(budget)
}

/// `--model`: every routed task goes to `alias` (`routing.policy = fixed`).
/// Named roles keep their `[roles]` binding.
fn pin_model(config: &mut KevinConfig, alias: &ModelAlias) -> anyhow::Result<()> {
    if !config.models.contains_key(alias) {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            format!("--model {alias}: no such alias in [models]"),
        )
        .into());
    }
    config.routing.policy = RoutingPolicy::Fixed;
    for kind in config.routing.kinds.values_mut() {
        kind.candidates = vec![alias.clone()];
    }
    config.roles.default = alias.clone();
    Ok(())
}

/// The goal text; in `--headless` mode piped stdin is appended as extra
/// context (interactive runs keep stdin for answers).
fn goal_text(args: &Args) -> anyhow::Result<String> {
    let mut text = args.goal.trim().to_owned();
    if text.is_empty() {
        return Err(ExitError::new(exit::INVALID_ARGS, "the goal must not be empty").into());
    }
    if args.headless && !render::stdin_is_tty() {
        let mut extra = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut extra)?;
        if !extra.trim().is_empty() {
            text.push_str("\n\n");
            text.push_str(extra.trim_end());
        }
    }
    Ok(text)
}

/// `--attach` files, registered as `file://` artifacts on the goal.
fn attachments(paths: &[PathBuf]) -> anyhow::Result<Vec<ArtifactRef>> {
    paths
        .iter()
        .map(|path| {
            let resolved = path.canonicalize().map_err(|e| {
                ExitError::new(exit::INVALID_ARGS, format!("--attach {}: {e}", path.display()))
            })?;
            let bytes = std::fs::metadata(&resolved).ok().map(|m| m.len());
            Ok(ArtifactRef {
                id: ArtifactId::new(),
                kind: ArtifactKind::File,
                uri: format!("file://{}", resolved.display()),
                sha256: None,
                bytes,
            })
        })
        .collect()
}
