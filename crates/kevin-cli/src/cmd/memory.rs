//! `kevin memory …` — memory store management (`plan/06-memory-and-learning.md`
//! §1.7). Owned by WS-18.
//!
//! Every subcommand runs embedded (memory is a local store, never proxied
//! through the API): the database comes from the resolved configuration and
//! the embedder from `[memory]`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Args as _;
use kevin_domain::{Actor, MemoryItemId, RunId};
use kevin_memory::{
    ContextBuilder, EmbedderKind, ExportItem, ForgetFilter, MemoryCfg, MemoryError, MemoryStore,
    NoopEmbedder, RepoId, ScopeFilter, SearchQuery, StoreRequest, embed, parse_kind,
};
use kevin_store::{Db, PgEventStore};

use crate::cmd::config::load_from_ctx;
use crate::{Ctx, ExitError, exit};

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

impl AddKind {
    const fn as_domain(self) -> kevin_domain::MemoryKind {
        match self {
            AddKind::Fact => kevin_domain::MemoryKind::Fact,
            AddKind::Preference => kevin_domain::MemoryKind::Preference,
        }
    }
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
        /// Only this repository's items (default: this repository + global).
        #[arg(long)]
        repo: bool,
        /// Print the block the planner would receive instead of the hit table.
        #[arg(long = "as-context")]
        as_context: bool,
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
        /// Items per batch.
        #[arg(long, value_name = "N", default_value_t = 64)]
        batch: usize,
    },
    /// Check the embedder and the memory schema.
    Doctor,
    /// Export every item as JSON.
    Export {
        /// JSON output (the only format for now).
        #[arg(long)]
        json: bool,
        /// Include the stored vectors (excluded by default).
        #[arg(long = "with-embeddings")]
        with_embeddings: bool,
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
    match args.cmd {
        Cmd::Search {
            query,
            kinds,
            top_k,
            repo,
            as_context,
        } => run_search(ctx, &query, &kinds, top_k, repo, as_context).await,
        Cmd::Add {
            kind,
            text,
            tag,
            global,
        } => run_add(ctx, kind, &text, tag, global).await,
        Cmd::Forget {
            item_id,
            run,
            repo,
            all_before,
        } => run_forget(ctx, item_id, run, repo, all_before).await,
        Cmd::Reindex { model, batch } => run_reindex(ctx, model, batch).await,
        Cmd::Doctor => run_doctor(ctx).await,
        Cmd::Export {
            json,
            with_embeddings,
        } => run_export(ctx, json, with_embeddings).await,
        Cmd::Import { file } => run_import(ctx, &file).await,
    }
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

async fn run_search(
    ctx: &Ctx,
    query: &str,
    kinds: &[String],
    top_k: Option<u32>,
    repo_only: bool,
    as_context: bool,
) -> anyhow::Result<ExitCode> {
    let store = open(ctx, None).await?;
    let repo = current_repo();
    let mut search = SearchQuery::new(query).with_scope(scope_filter(repo.as_ref(), repo_only));
    if let Some(k) = top_k {
        search = search.with_top_k(k as usize);
    }
    if !kinds.is_empty() {
        let mut parsed = Vec::with_capacity(kinds.len());
        for kind in kinds {
            parsed.push(parse_kind(kind).map_err(|e| ExitError::new(exit::INVALID_ARGS, e.to_string()))?);
        }
        search = search.with_kinds(parsed);
    }

    if as_context {
        let block = ContextBuilder::new(&store)
            .for_intake(query, repo.as_ref())
            .await
            .map_err(|e| memory_err(&e))?;
        if ctx.global.json {
            println!(
                "{}",
                serde_json::json!({
                    "text": block.text,
                    "refs": block.refs.iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "estimated_tokens": block.estimated_tokens,
                })
            );
        } else if block.is_empty() {
            println!("(nothing to inject)");
        } else {
            println!("{}", block.text);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let hits = store.search(search).await.map_err(|e| memory_err(&e))?;
    if ctx.global.json {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "id": hit.item.id.to_string(),
                    "short_id": hit.item.short_id(),
                    "kind": hit.item.kind.as_str(),
                    "scope": hit.item.scope.to_string(),
                    "content": hit.item.content,
                    "tags": hit.item.tags,
                    "importance": hit.item.importance,
                    "created_at": hit.item.created_at,
                    "source": hit.item.source,
                    "score": hit.score,
                    "similarity": hit.similarity,
                    "lexical": hit.lexical,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "hits": rows }));
        return Ok(ExitCode::SUCCESS);
    }
    if hits.is_empty() {
        println!("no memory item matches `{query}`");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{:<8} {:<16} {:<7} {:<6} {:<6} {:<6} CONTENT",
        "ID", "KIND", "SCOPE", "SCORE", "SIM", "LEX"
    );
    for hit in &hits {
        println!(
            "{:<8} {:<16} {:<7} {:<6.2} {:<6.2} {:<6.2} {}",
            hit.item.short_id(),
            hit.item.kind.as_str(),
            kevin_memory::item::scope_label(&hit.item.scope),
            hit.score,
            hit.similarity,
            hit.lexical,
            one_line(&hit.item.content),
        );
        if ctx.global.verbose > 0 {
            println!(
                "         id={} tags={:?} importance={:.2} model={} source={}",
                hit.item.id,
                hit.item.tags,
                hit.item.importance,
                hit.item.embedding_model.as_deref().unwrap_or("none"),
                provenance(&hit.item.source),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_add(
    ctx: &Ctx,
    kind: AddKind,
    text: &str,
    tags: Vec<String>,
    global: bool,
) -> anyhow::Result<ExitCode> {
    let store = open(ctx, None).await?;
    let scope = match (global, current_repo()) {
        (false, Some(repo)) => repo.scope(),
        _ => kevin_memory::MemoryScope::Global,
    };
    let request = StoreRequest::new(kind.as_domain(), text)
        .with_tags(tags)
        .with_scope(scope.clone())
        .with_source(kevin_memory::MemorySource::from_actor(Actor::user(
            whoami(),
        )));
    let id = store.store(request).await.map_err(|e| memory_err(&e))?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "id": id.to_string(), "kind": kind.as_domain().as_str(), "scope": scope.to_string() })
        );
    } else if !ctx.global.quiet {
        println!("stored {} ({}, scope {scope})", id, kind.as_domain());
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_forget(
    ctx: &Ctx,
    item_id: Option<MemoryItemId>,
    run: Option<RunId>,
    repo: Option<String>,
    all_before: Option<String>,
) -> anyhow::Result<ExitCode> {
    let store = open(ctx, None).await?;
    let actor = Actor::user(whoami());
    let filter = match (item_id, run, repo, all_before) {
        (Some(id), _, _, _) => ForgetFilter::Id(id),
        (_, Some(run), _, _) => ForgetFilter::Run(run),
        (_, _, Some(scope), _) => ForgetFilter::Repo(RepoId::from_hex(
            scope.strip_prefix("repo:").unwrap_or(&scope).to_owned(),
        )),
        (_, _, _, Some(date)) => ForgetFilter::before_rfc3339(&date).map_err(|e| memory_err(&e))?,
        _ => {
            return Err(ExitError::new(
                exit::INVALID_ARGS,
                "pass an item id, --run, --repo or --all-before",
            )
            .into());
        }
    };
    let forgotten = store
        .forget_matching(&filter, actor)
        .await
        .map_err(|e| memory_err(&e))?;
    if ctx.global.json {
        println!("{}", serde_json::json!({ "forgotten": forgotten }));
    } else if !ctx.global.quiet {
        println!("forgot {forgotten} memory item(s)");
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_reindex(
    ctx: &Ctx,
    model: Option<String>,
    batch: usize,
) -> anyhow::Result<ExitCode> {
    let store = open(ctx, model.as_deref()).await?;
    let quiet = ctx.global.quiet || ctx.global.json;
    let report = store
        .reindex(batch, |done, total| {
            if !quiet {
                println!("re-embedded {done}/{total}");
            }
        })
        .await
        .map_err(|e| memory_err(&e))?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "embedded": report.embedded,
                "total": report.total,
                "model": report.model,
            })
        );
    } else if !ctx.global.quiet {
        println!(
            "reindex done: {} item(s) now embedded with `{}`",
            report.embedded, report.model
        );
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_doctor(ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = resolve(ctx)?;
    let cfg = MemoryCfg::from_config(&resolved);
    let pool = Db::connect(&resolved.database)
        .await
        .map_err(|e| ExitError::new(exit::UNREACHABLE, e.to_string()))?;

    // Loading the embedder is the pre-fetch step: it downloads the model once.
    let embedder = embed::embedder_from_cfg(&cfg).await;
    let embedder_status = match &embedder {
        Ok(embedder) => format!(
            "ok (model `{}`, {} dimensions)",
            embedder.model_name(),
            embedder.dimensions()
        ),
        Err(err) => format!("FAILED: {err}"),
    };
    let store = MemoryStore::new(
        pool.clone(),
        embedder.unwrap_or_else(|_| Arc::new(NoopEmbedder::new(cfg.dimensions))),
        cfg.clone(),
    );
    let column = store.column_dimensions().await.map_err(|e| memory_err(&e))?;
    let index = store.hnsw_index_present().await.map_err(|e| memory_err(&e))?;
    let counts = store.counts().await.map_err(|e| memory_err(&e))?;
    let dimensions_ok = column == cfg.dimensions;
    pool.close().await;

    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "enabled": cfg.enabled,
                "embedder": cfg.embedder.as_str(),
                "embedding_model": cfg.embedding_model,
                "embedder_status": embedder_status,
                "model_cache_dir": cfg.model_cache_dir,
                "configured_dimensions": cfg.dimensions,
                "column_dimensions": column,
                "dimensions_ok": dimensions_ok,
                "hnsw_index": index,
                "live": counts.live,
                "forgotten": counts.forgotten,
                "pending_embedding": counts.pending_embedding,
                "by_kind": counts.by_kind,
                "models": counts.models,
            })
        );
    } else {
        println!("memory.enabled:      {}", cfg.enabled);
        println!("embedder:            {} ({})", cfg.embedder.as_str(), embedder_status);
        println!("model cache:         {}", cfg.model_cache_dir.display());
        println!(
            "dimensions:          config {} / column {} — {}",
            cfg.dimensions,
            column,
            if dimensions_ok { "ok" } else { "MISMATCH" }
        );
        println!(
            "hnsw index:          {}",
            if index { "present" } else { "MISSING" }
        );
        println!("items (live):        {}", counts.live);
        for (kind, n) in &counts.by_kind {
            println!("  {kind:<18} {n}");
        }
        println!("forgotten:           {}", counts.forgotten);
        println!("pending embedding:   {}", counts.pending_embedding);
        println!(
            "embedding models:    {}",
            if counts.models.is_empty() {
                "(none)".to_owned()
            } else {
                counts.models.join(", ")
            }
        );
    }
    Ok(if dimensions_ok && index {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit::FAILED)
    })
}

async fn run_export(ctx: &Ctx, _json: bool, with_embeddings: bool) -> anyhow::Result<ExitCode> {
    let store = open(ctx, None).await?;
    let items = store.export(with_embeddings).await.map_err(|e| memory_err(&e))?;
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(ExitCode::SUCCESS)
}

async fn run_import(ctx: &Ctx, file: &PathBuf) -> anyhow::Result<ExitCode> {
    let text = std::fs::read_to_string(file).map_err(|e| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!("cannot read {}: {e}", file.display()),
        )
    })?;
    let items: Vec<ExportItem> = serde_json::from_str(&text).map_err(|e| {
        ExitError::new(
            exit::INVALID_ARGS,
            format!("{} is not a `kevin memory export` file: {e}", file.display()),
        )
    })?;
    let store = open(ctx, None).await?;
    let report = store.import(&items).await.map_err(|e| memory_err(&e))?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "imported": report.imported,
                "skipped": report.skipped,
                "refused": report.refused.iter()
                    .map(|(id, why)| serde_json::json!({"id": id.to_string(), "reason": why}))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "imported {}, skipped {} (already present), refused {}",
            report.imported,
            report.skipped,
            report.refused.len()
        );
        for (id, why) in &report.refused {
            eprintln!("  refused {id}: {why}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by `kevin lessons`)
// ---------------------------------------------------------------------------

/// Resolves the configuration for a memory command.
pub(crate) fn resolve(ctx: &Ctx) -> anyhow::Result<kevin_config::KevinConfig> {
    let resolved = load_from_ctx(ctx)
        .map_err(|errors| ExitError::new(exit::INVALID_ARGS, format!("configuration: {errors}")))?;
    Ok(resolved.config)
}

/// Opens a memory store from the resolved configuration; `model` overrides
/// `memory.embedding_model` for this invocation (`kevin memory reindex --model`).
pub(crate) async fn open(ctx: &Ctx, model: Option<&str>) -> anyhow::Result<MemoryStore> {
    let config = resolve(ctx)?;
    let mut cfg = MemoryCfg::from_config(&config);
    if let Some(model) = model {
        cfg = cfg.with_embedder(EmbedderKind::Fastembed, model);
    }
    if !cfg.enabled {
        return Err(ExitError::new(
            exit::INVALID_ARGS,
            "memory is disabled (`memory.enabled = false`)",
        )
        .into());
    }
    let pool = Db::connect(&config.database)
        .await
        .map_err(|e| ExitError::new(exit::UNREACHABLE, e.to_string()))?;
    let embedder = embed::embedder_from_cfg(&cfg)
        .await
        .map_err(|e| ExitError::new(exit::FAILED, e.to_string()))?;
    let events = Arc::new(PgEventStore::new(pool.clone()));
    Ok(MemoryStore::new(pool, embedder, cfg).with_events(events))
}

/// `repo:<id>` of the current working directory, when it is a git repository.
pub(crate) fn current_repo() -> Option<RepoId> {
    let origin = git(&["remote", "get-url", "origin"]);
    if let Some(url) = origin.filter(|u| !u.is_empty()) {
        return Some(RepoId::from_origin(&url));
    }
    let root = git(&["rev-parse", "--show-toplevel"])?;
    Some(RepoId::from_path(std::path::Path::new(&root)))
}

/// Search scope: this repository plus global, or this repository only.
pub(crate) fn scope_filter(repo: Option<&RepoId>, repo_only: bool) -> ScopeFilter {
    match (repo, repo_only) {
        (Some(repo), true) => ScopeFilter::Repo(repo.clone()),
        (Some(repo), false) => ScopeFilter::RepoAndGlobal(repo.clone()),
        (None, _) => ScopeFilter::Global,
    }
}

/// The operator's name, for `MemorySource.actor`.
pub(crate) fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "operator".to_owned())
}

pub(crate) fn memory_err(err: &MemoryError) -> anyhow::Error {
    let code = if err.is_unreachable() {
        exit::UNREACHABLE
    } else if matches!(err, MemoryError::Disabled | MemoryError::Invalid(_)) {
        exit::INVALID_ARGS
    } else {
        exit::FAILED
    };
    ExitError::new(code, err.to_string()).into()
}

fn provenance(source: &kevin_memory::MemorySource) -> String {
    let mut parts = Vec::new();
    if let Some(run) = source.run_id {
        parts.push(format!("run={run}"));
    }
    if let Some(task) = source.task_id {
        parts.push(format!("task={task}"));
    }
    if let Some(evaluation) = source.evaluation_id {
        parts.push(format!("evaluation={evaluation}"));
    }
    parts.push(format!("actor={:?}", source.actor));
    parts.join(" ")
}

fn one_line(text: &str) -> String {
    let single = text.replace('\n', " ");
    if single.chars().count() <= 96 {
        single
    } else {
        format!("{}…", single.chars().take(95).collect::<String>())
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_filters_follow_the_repo_flag() {
        let repo = RepoId::from_origin("https://example.com/x");
        assert_eq!(
            scope_filter(Some(&repo), false),
            ScopeFilter::RepoAndGlobal(repo.clone())
        );
        assert_eq!(scope_filter(Some(&repo), true), ScopeFilter::Repo(repo));
        assert_eq!(scope_filter(None, true), ScopeFilter::Global);
    }

    #[test]
    fn long_content_is_shortened_for_the_table() {
        assert_eq!(one_line("a\nb"), "a b");
        assert!(one_line(&"x".repeat(200)).ends_with('…'));
    }
}
