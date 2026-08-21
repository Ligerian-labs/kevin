//! `kevin memory …` — memory store management. Owned by WS-18.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Args as _;
use kevin_domain::{MemoryItemId, RunId};

use crate::{Ctx, not_implemented};

/// Subcommand name.
pub const NAME: &str = "memory";

/// Arguments of `kevin memory`.
#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// What to do.
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// Kinds an operator may add by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AddKind {
    /// A durable fact.
    Fact,
    /// A user preference.
    Preference,
}

/// `kevin memory` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Hybrid (vector + text) search.
    Search {
        /// Query text.
        query: String,
        /// Restrict to these kinds (comma-separated).
        #[arg(long, value_name = "KINDS", value_delimiter = ',')]
        kinds: Vec<String>,
        /// Number of hits.
        #[arg(long = "top-k", value_name = "N")]
        top_k: Option<u32>,
    },
    /// Add a fact or preference.
    Add {
        /// Item kind.
        #[arg(long, value_enum)]
        kind: AddKind,
        /// Item text.
        text: String,
        /// Tag (repeatable).
        #[arg(long = "tag", value_name = "TAG", action = clap::ArgAction::Append)]
        tag: Vec<String>,
        /// Global scope instead of the current repository.
        #[arg(long)]
        global: bool,
    },
    /// Forget items (one id, or a whole scope).
    Forget {
        /// Item id.
        #[arg(value_name = "ITEM_ID", group = "target")]
        item_id: Option<MemoryItemId>,
        /// Everything learned from this run.
        #[arg(long, value_name = "RUN_ID", group = "target")]
        run: Option<RunId>,
        /// Everything scoped to this repository.
        #[arg(long, value_name = "SCOPE", group = "target")]
        repo: Option<String>,
        /// Everything created before this date (RFC 3339).
        #[arg(long, value_name = "DATE", group = "target")]
        all_before: Option<String>,
    },
    /// Recompute embeddings (e.g. after changing the model).
    Reindex {
        /// Embedding model to use.
        #[arg(long, value_name = "MODEL")]
        model: Option<String>,
    },
    /// Check the embedder and the memory schema.
    Doctor,
    /// Export every item as JSON.
    Export {
        /// JSON output (the only format for now).
        #[arg(long)]
        json: bool,
    },
    /// Import items from a JSON export.
    Import {
        /// Export file.
        file: PathBuf,
    },
}

/// The `kevin memory` command definition.
#[must_use]
pub fn command() -> clap::Command {
    let cmd = clap::Command::new(NAME);
    Args::augment_args(cmd)
        .about("Memory store: search, add, forget, reindex, export")
        .mut_subcommand("forget", |c| {
        c.group(clap::ArgGroup::new("target").required(true).multiple(false))
    })
}

/// Runs `kevin memory`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let _ = (args, ctx);
    Err(not_implemented(NAME))
}
