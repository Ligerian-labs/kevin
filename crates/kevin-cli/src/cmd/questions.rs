//! `kevin questions …` — the question inbox. Owned by WS-12.

use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::RunId;
use kevin_orchestrator::projections::{QuestionInboxRow, QuestionQuery};

use crate::{Ctx, embedded, render};

/// Subcommand name.
pub const NAME: &str = "questions";

/// Status of the questions the inbox shows by default.
pub const OPEN: &str = "open";

/// Arguments of `kevin questions`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// `kevin questions` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// List open questions.
    Ls {
        /// Only questions of this run.
        #[arg(long, value_name = "RUN_ID")]
        run: Option<RunId>,
        /// Also list answered and expired questions.
        #[arg(long)]
        all: bool,
    },
}

/// The `kevin questions` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("List open questions")
}

/// Runs `kevin questions`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let Cmd::Ls { run, all } = args.cmd;
    let backend = embedded::open_backend(ctx).await?;
    let query = QuestionQuery {
        run_id: run.map(|r| r.as_uuid()),
        status: (!all).then(|| OPEN.to_owned()),
        cursor: None,
        limit: None,
    };
    let page = backend.read_models().questions(&query).await;
    backend.close().await;
    let page = page?;

    if ctx.global.json {
        render::json_line(&serde_json::json!({
            "questions": page.items.iter().map(question_json).collect::<Vec<_>>(),
            "next_cursor": page.next_cursor,
        }));
    } else if page.items.is_empty() {
        render::line("no open questions");
    } else {
        let rows = page
            .items
            .iter()
            .map(|q| {
                vec![
                    render::short(q.question_id),
                    render::short(q.run_id),
                    q.status.clone(),
                    excerpt(&q.text),
                    labels(q).join(" | "),
                    render::age(q.asked_at),
                ]
            })
            .collect();
        render::line(&render::table(
            &["question", "run", "status", "text", "options", "age"],
            rows,
        ));
    }
    Ok(ExitCode::SUCCESS)
}

/// The `QuestionDto`-shaped JSON of one inbox row.
#[must_use]
pub fn question_json(q: &QuestionInboxRow) -> serde_json::Value {
    serde_json::json!({
        "id": q.question_id,
        "run_id": q.run_id,
        "task_id": q.task_id,
        "text": q.text,
        "options": q.options,
        "multi_select": q.multi_select,
        "default": q.default_answer,
        "policy": q.policy,
        "status": q.status,
        "answer": q.answer,
        "answered_by": q.answered_by,
        "asked_at": q.asked_at,
    })
}

/// Option labels of a question, in order.
#[must_use]
pub fn labels(q: &QuestionInboxRow) -> Vec<String> {
    q.options
        .as_array()
        .map(|options| {
            options
                .iter()
                .filter_map(|o| o.get("label").and_then(|l| l.as_str()))
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn excerpt(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= 60 {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(59).collect::<String>())
    }
}
