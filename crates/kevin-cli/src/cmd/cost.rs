//! `kevin cost` — spend report. Owned by WS-12.

use std::process::ExitCode;
use std::time::Duration;

use chrono::Utc;
use clap::Args as _;
use kevin_domain::RunId;
use kevin_orchestrator::projections::{CostGroupBy, CostQuery};

use crate::{Ctx, ExitError, embedded, exit, render};

/// Subcommand name.
pub const NAME: &str = "cost";

/// Grouping dimension of the cost report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GroupBy {
    /// Per run.
    Run,
    /// Per model alias.
    Model,
    /// Per task kind.
    Kind,
}

impl From<GroupBy> for CostGroupBy {
    fn from(value: GroupBy) -> Self {
        match value {
            GroupBy::Run => CostGroupBy::Run,
            GroupBy::Model => CostGroupBy::Model,
            GroupBy::Kind => CostGroupBy::Kind,
        }
    }
}

/// Arguments of `kevin cost`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Only spend since this long ago (e.g. `7d`, `24h`).
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,
    /// Group rows by run, model or kind.
    #[arg(long, value_enum, value_name = "DIM")]
    pub group_by: Option<GroupBy>,
    /// Only this run.
    #[arg(long, value_name = "RUN_ID")]
    pub run: Option<RunId>,
}

/// The `kevin cost` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME))
        .about("Spend report (USD, tokens) from the cost ledger")
}

/// Runs `kevin cost`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let since = args.since.as_deref().map(parse_since).transpose()?;
    let backend = embedded::open_backend(ctx).await?;
    let query = CostQuery {
        since: since.map(|d| {
            Utc::now() - chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::zero())
        }),
        run_id: args.run.map(|r| r.as_uuid()),
        group_by: args.group_by.map(CostGroupBy::from).unwrap_or_default(),
    };
    let report = backend.read_models().cost(&query).await;
    backend.close().await;
    let report = report?;

    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "total_usd": report.total_usd.map(|d| d.to_string()),
            "total_tokens": report.total_tokens,
            "group_by": query.group_by.as_str(),
            "rows": report.rows.iter().map(|row| serde_json::json!({
                "key": row.key,
                "usd": row.usd.map(|d| d.to_string()),
                "input_tokens": row.input_tokens,
                "output_tokens": row.output_tokens,
                "attempts": row.attempts,
            })).collect::<Vec<_>>(),
        }));
    } else if report.rows.is_empty() {
        render::line("no spend recorded");
    } else {
        let rows = report
            .rows
            .iter()
            .map(|row| {
                vec![
                    row.key.clone(),
                    render::money(row.usd),
                    row.input_tokens.to_string(),
                    row.output_tokens.to_string(),
                    row.attempts.to_string(),
                ]
            })
            .collect();
        render::line(&render::table(
            &[query.group_by.as_str(), "usd", "in", "out", "attempts"],
            rows,
        ));
        render::line(&format!(
            "total: {} USD, {} tokens",
            render::money(report.total_usd),
            report.total_tokens
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// Parses `--since` (`7d`, `24h`, `90m`, `30s`).
fn parse_since(raw: &str) -> anyhow::Result<Duration> {
    humantime::parse_duration(raw)
        .map_err(|e| ExitError::new(exit::INVALID_ARGS, format!("--since {raw}: {e}")).into())
}
