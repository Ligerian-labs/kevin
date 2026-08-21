//! `kevin lessons` — learned lessons from memory (`memory.lessons_view`,
//! `plan/06-memory-and-learning.md` §1.7). Owned by WS-18.

use std::process::ExitCode;

use clap::Args as _;

use crate::Ctx;
use crate::cmd::memory::{current_repo, memory_err, open, scope_filter};

/// Subcommand name.
pub const NAME: &str = "lessons";

/// Default number of lessons printed.
const DEFAULT_LIMIT: u32 = 20;

/// Arguments of `kevin lessons`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Maximum number of lessons.
    #[arg(long, value_name = "N")]
    pub limit: Option<u32>,
    /// Only lessons scoped to the current repository.
    #[arg(long)]
    pub repo: bool,
}

/// The `kevin lessons` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("List lessons learned from evaluations")
}

/// Runs `kevin lessons`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let store = open(ctx, None).await?;
    let scope = scope_filter(current_repo().as_ref(), args.repo);
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT) as usize;
    let lessons = store.lessons(&scope, limit).await.map_err(|e| memory_err(&e))?;

    if ctx.global.json {
        println!("{}", serde_json::to_string(&lessons)?);
        return Ok(ExitCode::SUCCESS);
    }
    if lessons.is_empty() {
        println!("no lessons yet (they are stored when evaluations record them)");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<38} {:<12} {:<6} {:<10} LESSON",
        "ID", "SCOPE", "IMP", "LEARNED"
    );
    for lesson in &lessons {
        println!(
            "{:<38} {:<12} {:<6.2} {:<10} {}",
            lesson.id,
            kevin_memory::item::scope_label(&lesson.scope),
            lesson.importance,
            lesson.created_at.format("%Y-%m-%d"),
            lesson.content.replace('\n', " "),
        );
        if ctx.global.verbose > 0 {
            println!(
                "    tags={:?} run={}",
                lesson.tags,
                lesson.run_id.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}
