//! `kevin proposals …` — the inbox of prompt/config/routing changes raised by
//! evaluations (`plan/06-memory-and-learning.md` §3.4,
//! `plan/07-api-and-tui.md` §3). Owned by WS-19.
//!
//! Runs embedded: the inbox is a projection in the local database, never
//! proxied through the API. Accepting a **routing** proposal applies it (a
//! route outcome is bounded and self-correcting); prompt and config proposals
//! are printed for a human to apply — Kevin never writes them
//! (`plan/adr/0010-evaluation-auto-apply-policy.md`).

use std::process::ExitCode;
use std::sync::Arc;

use clap::Args as _;
use kevin_domain::{ProposalId, ProposalStatus};
use kevin_evaluator::proposals::DEFAULT_LIMIT;
use kevin_evaluator::repo::{kind_str, status_str};
use kevin_evaluator::{EvaluatorError, PgEvaluationRepo, ProposalRow, Proposals};
use kevin_router::{PgRouteScoreRepo, Router};
use kevin_store::{Db, PgEventStore};

use crate::cmd::memory::{resolve, whoami};
use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "proposals";

/// Arguments of `kevin proposals`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Status filter of `kevin proposals ls`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum StatusFilter {
    /// Waiting for a human (the default).
    #[default]
    Proposed,
    /// Already accepted.
    Accepted,
    /// Already rejected.
    Rejected,
    /// Every proposal, whatever its status.
    All,
}

impl StatusFilter {
    const fn as_status(self) -> Option<ProposalStatus> {
        match self {
            StatusFilter::Proposed => Some(ProposalStatus::Proposed),
            StatusFilter::Accepted => Some(ProposalStatus::Accepted),
            StatusFilter::Rejected => Some(ProposalStatus::Rejected),
            StatusFilter::All => None,
        }
    }
}

/// `kevin proposals` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List proposals (default: the ones waiting for a decision).
    Ls {
        /// Which proposals to list.
        #[arg(long, value_enum, default_value_t = StatusFilter::Proposed)]
        status: StatusFilter,
        /// Maximum number of rows.
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
    },
    /// Show one proposal in full, with the evaluation that raised it.
    Show {
        /// Proposal id.
        id: ProposalId,
    },
    /// Accept a proposal (routing proposals are applied; others are for humans).
    Accept {
        /// Proposal id.
        id: ProposalId,
        /// Why, recorded on `evaluation.proposal_accepted`.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// Reject a proposal.
    Reject {
        /// Proposal id.
        id: ProposalId,
        /// Why, recorded on `evaluation.proposal_rejected` so the decision is
        /// auditable later.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
}

/// The `kevin proposals` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Review proposals raised by evaluations")
}

/// Runs `kevin proposals`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let inbox = open(ctx).await?;
    match args.cmd {
        Cmd::Ls { status, limit } => {
            let limit = limit.map_or(DEFAULT_LIMIT, |n| n as usize);
            let rows = inbox
                .list(status.as_status(), limit)
                .await
                .map_err(|e| evaluator_err(&e))?;
            print_list(&rows, ctx);
        }
        Cmd::Show { id } => {
            let row = inbox.get(id).await.map_err(|e| evaluator_err(&e))?;
            print_show(&row, ctx);
        }
        Cmd::Accept { id, note } => {
            let who = whoami();
            let outcome = inbox
                .accept(id, &who, note.clone())
                .await
                .map_err(|e| evaluator_err(&e))?;
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "proposal": dto(&outcome.proposal),
                        "applied": outcome.applied,
                        "manual": outcome.manual,
                        "note": note,
                    })
                );
            } else {
                println!("accepted {} ({})", outcome.proposal.id, kind_str(outcome.proposal.kind));
                if outcome.applied {
                    println!("applied to routing.");
                }
                if let Some(manual) = &outcome.manual {
                    println!("\nApply this yourself:\n{manual}\n\n{}", outcome.proposal.body);
                }
            }
        }
        Cmd::Reject { id, note } => {
            let who = whoami();
            let row = inbox
                .reject(id, &who, note.clone())
                .await
                .map_err(|e| evaluator_err(&e))?;
            if ctx.global.json {
                println!(
                    "{}",
                    serde_json::json!({ "proposal": dto(&row), "note": note })
                );
            } else {
                println!("rejected {} ({})", row.id, kind_str(row.kind));
                if let Some(note) = note {
                    println!("note: {note}");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Opens the inbox over the configured database.
async fn open(ctx: &Ctx) -> anyhow::Result<Proposals> {
    let config = resolve(ctx)?;
    let pool = Db::connect(&config.database)
        .await
        .map_err(|e| ExitError::new(exit::UNREACHABLE, e.to_string()))?;
    let events = Arc::new(PgEventStore::new(pool.clone()));
    let scores = Arc::new(PgRouteScoreRepo::new(pool.clone()));
    let router = Arc::new(Router::from_config(&config, scores));
    let repo = Arc::new(PgEvaluationRepo::new(pool, events));
    Ok(Proposals::new(repo).with_router(router))
}

fn print_list(rows: &[ProposalRow], ctx: &Ctx) {
    if ctx.global.json {
        let items: Vec<serde_json::Value> = rows.iter().map(dto).collect();
        println!("{}", serde_json::json!({ "items": items }));
        return;
    }
    if rows.is_empty() {
        println!("no proposals (they are raised when evaluations record them)");
        return;
    }
    println!("{:<38} {:<8} {:<9} {:<10} BODY", "ID", "KIND", "STATUS", "RAISED");
    for row in rows {
        println!(
            "{:<38} {:<8} {:<9} {:<10} {}",
            row.id,
            kind_str(row.kind),
            status_str(row.status),
            row.created_at.format("%Y-%m-%d"),
            one_line(&row.body),
        );
    }
}

fn print_show(row: &ProposalRow, ctx: &Ctx) {
    if ctx.global.json {
        println!("{}", serde_json::json!(dto(row)));
        return;
    }
    println!("id:         {}", row.id);
    println!("kind:       {}", kind_str(row.kind));
    println!("status:     {}", status_str(row.status));
    println!("evaluation: {}", row.evaluation_id);
    println!("run:        {}", row.run_id);
    println!("raised:     {}", row.created_at.to_rfc3339());
    if let Some(by) = &row.decided_by {
        println!(
            "decided:    {by} at {}",
            row.decided_at.map(|t| t.to_rfc3339()).unwrap_or_default()
        );
    }
    println!("\nrationale:\n{}", row.rationale);
    println!("\nbody:\n{}", row.body);
}

/// `ProposalDto` of `plan/07-api-and-tui.md` §DTOs.
fn dto(row: &ProposalRow) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "evaluation_id": row.evaluation_id,
        "run_id": row.run_id,
        "kind": kind_str(row.kind),
        "body": row.body,
        "rationale": row.rationale,
        "status": status_str(row.status),
        "decided_by": row.decided_by,
        "decided_at": row.decided_at,
        "created_at": row.created_at,
    })
}

fn one_line(text: &str) -> String {
    let single = text.replace('\n', " ");
    if single.chars().count() <= 72 {
        single
    } else {
        format!("{}…", single.chars().take(71).collect::<String>())
    }
}

fn evaluator_err(err: &EvaluatorError) -> anyhow::Error {
    let code = match err {
        EvaluatorError::ProposalNotFound(_) | EvaluatorError::EvaluationNotFound(_) => {
            exit::INVALID_ARGS
        }
        EvaluatorError::Store(_) => exit::UNREACHABLE,
        _ => exit::FAILED,
    };
    ExitError::new(code, err.to_string()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_filter_maps_to_the_projection_values() {
        assert_eq!(
            StatusFilter::Proposed.as_status(),
            Some(ProposalStatus::Proposed)
        );
        assert_eq!(StatusFilter::All.as_status(), None);
        assert_eq!(status_str(ProposalStatus::Accepted), "accepted");
    }

    #[test]
    fn long_bodies_are_shortened_for_the_table() {
        assert_eq!(one_line("a\nb"), "a b");
        assert!(one_line(&"x".repeat(200)).ends_with('…'));
    }

    #[test]
    fn the_command_exposes_the_documented_subcommands() {
        let names: Vec<String> = command()
            .get_subcommands()
            .map(|c| c.get_name().to_owned())
            .collect();
        assert_eq!(names, ["ls", "show", "accept", "reject"]);
    }
}
