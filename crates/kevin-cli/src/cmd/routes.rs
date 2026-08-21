//! `kevin routes …` — route leaderboard, dry-run explanation and reset
//! (`plan/06-memory-and-learning.md` §2.5). Owned by WS-09.
//!
//! Every subcommand talks to Postgres directly (the routing tables are the
//! router's own read model): `routes` prints `routing.route_leaderboard`,
//! `explain` runs a dry-run `Router::select` without recording anything, and
//! `reset` puts a `(kind, alias)` pair back on its tier prior and appends the
//! resulting `routing.score_updated` event.

use std::process::ExitCode;
use std::sync::Arc;

use chrono::Utc;
use clap::Args as _;
use kevin_domain::{Actor, Complexity, EventId, ModelAlias, RouteScore, TaskKind};
use kevin_router::{
    CatalogRepo, ModelCatalog, PgRouteScoreRepo, Router, RouteScoreUpdated, SelectRouteQuery,
    render_explain, render_leaderboard,
};
use kevin_store::{Db, EventStore, NewEvent, PgEventStore, StreamId};

use crate::cmd::config::load_from_ctx;
use crate::{Ctx, ExitError, exit};

/// Subcommand name.
pub const NAME: &str = "routes";

/// Arguments of `kevin routes` (no subcommand = leaderboard).
#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    /// Only this task kind.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<TaskKind>,
    /// Database URL for this invocation (default: the resolved configuration).
    #[arg(long, value_name = "URL", global = true)]
    pub url: Option<String>,
    /// What to do (default: show the leaderboard).
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

/// `kevin routes` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum Cmd {
    /// Explain which route would be selected for a task kind (dry run).
    Explain {
        /// Task kind.
        #[arg(long, value_name = "KIND")]
        kind: TaskKind,
        /// Estimated complexity (low|medium|high).
        #[arg(long, value_name = "C", default_value = "medium")]
        complexity: Complexity,
        /// Alias to exclude, repeatable (simulates a retry).
        #[arg(long, value_name = "ALIAS", action = clap::ArgAction::Append)]
        exclude: Vec<ModelAlias>,
        /// Seed the sampler so the explanation is reproducible.
        #[arg(long, value_name = "N", default_value_t = 0)]
        seed: u64,
    },
    /// Reset learned scores back to their tier priors.
    Reset {
        /// Only this task kind (default: every kind).
        #[arg(long, value_name = "KIND")]
        kind: Option<TaskKind>,
        /// Only this alias (default: every alias).
        #[arg(long, value_name = "ALIAS")]
        alias: Option<ModelAlias>,
    },
}

/// The `kevin routes` command definition.
#[must_use]
pub fn command() -> clap::Command {
    Args::augment_args(clap::Command::new(NAME)).about("Route leaderboard, explain and reset")
}

/// Runs `kevin routes`.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let resolved = load_from_ctx(ctx)
        .map_err(|errors| ExitError::new(exit::INVALID_ARGS, format!("configuration: {errors}")))?;
    let url = match args.url.clone() {
        Some(url) => url,
        None => kevin_store::DatabaseCfg::from_config(&resolved.config.database)
            .map_err(|e| store_err(&e))?
            .url,
    };
    let pool = Db::connect_url(&url).await.map_err(|e| store_err(&e))?;
    let catalog = Arc::new(ModelCatalog::from_config(&resolved.config));
    let repo = Arc::new(PgRouteScoreRepo::new(pool.clone()));
    let router = Router::new(Arc::clone(&catalog), &resolved.config, repo);

    let result = match args.cmd.clone() {
        None => leaderboard(&router, &pool, &catalog, args.kind.as_ref(), ctx).await,
        Some(Cmd::Explain {
            kind,
            complexity,
            exclude,
            seed,
        }) => explain(&router, &pool, &catalog, kind, complexity, exclude, seed, ctx).await,
        Some(Cmd::Reset { kind, alias }) => {
            reset(&router, &pool, kind.as_ref(), alias.as_ref(), ctx).await
        }
    };
    pool.close().await;
    result
}

async fn leaderboard(
    router: &Router,
    pool: &kevin_store::PgPool,
    catalog: &ModelCatalog,
    kind: Option<&TaskKind>,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    sync_catalog(pool, catalog).await?;
    let rows = router.leaderboard(kind).await.map_err(|e| routing_err(&e))?;
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({
                "catalog_version": catalog.version(),
                "routes": rows,
            })
        );
    } else {
        print!("{}", render_leaderboard(&rows, Utc::now()));
    }
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
async fn explain(
    router: &Router,
    pool: &kevin_store::PgPool,
    catalog: &ModelCatalog,
    kind: TaskKind,
    complexity: Complexity,
    exclude: Vec<ModelAlias>,
    seed: u64,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    sync_catalog(pool, catalog).await?;
    let query = SelectRouteQuery::new(kind)
        .complexity(complexity)
        .exclude(exclude)
        .rng_seed(seed);
    let selection = router.select(query).await.map_err(|e| routing_err(&e))?;
    if ctx.global.json {
        println!("{}", serde_json::to_string(&selection)?);
    } else {
        print!("{}", render_explain(&selection));
    }
    Ok(ExitCode::SUCCESS)
}

async fn reset(
    router: &Router,
    pool: &kevin_store::PgPool,
    kind: Option<&TaskKind>,
    alias: Option<&ModelAlias>,
    ctx: &Ctx,
) -> anyhow::Result<ExitCode> {
    let updates = router.reset(kind, alias).await.map_err(|e| routing_err(&e))?;
    let store = PgEventStore::new(pool.clone());
    for update in &updates {
        append_score_updated(&store, update).await?;
    }
    if ctx.global.json {
        println!(
            "{}",
            serde_json::json!({ "reset": updates.len(), "routes": updates })
        );
    } else if updates.is_empty() {
        println!("nothing to reset");
    } else {
        for update in &updates {
            println!(
                "reset {} / {} to prior Beta({}, {})",
                update.task_kind, update.alias, update.stats.alpha, update.stats.beta
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Appends the `routing.score_updated` event of a reset to the event store.
async fn append_score_updated(
    store: &PgEventStore,
    update: &RouteScoreUpdated,
) -> anyhow::Result<()> {
    let stream = StreamId::new(
        RouteScoreUpdated::AGGREGATE_TYPE,
        RouteScore::id_for(&update.task_kind, &update.alias),
    );
    let current = store
        .load_stream(&stream, 0)
        .await
        .map_err(|e| store_err(&e))?;
    let event = NewEvent {
        event_id: EventId::new(),
        event_type: RouteScoreUpdated::EVENT_TYPE,
        schema_version: 1,
        occurred_at: Utc::now(),
        correlation_id: stream.aggregate_id,
        causation_id: None,
        actor: Actor::system("cli"),
        payload: serde_json::to_value(update.to_event())?,
    };
    store
        .append(&stream, current.len() as u64, &[event])
        .await
        .map_err(|e| store_err(&e))?;
    Ok(())
}

async fn sync_catalog(pool: &kevin_store::PgPool, catalog: &ModelCatalog) -> anyhow::Result<()> {
    CatalogRepo::new(pool.clone())
        .sync(catalog)
        .await
        .map_err(|e| routing_err(&e))?;
    Ok(())
}

fn routing_err(err: &kevin_router::RoutingError) -> anyhow::Error {
    let code = match err {
        kevin_router::RoutingError::Store(store) if store.is_unreachable() => exit::UNREACHABLE,
        kevin_router::RoutingError::NoRoute { .. }
        | kevin_router::RoutingError::UnknownAlias { .. } => exit::INVALID_ARGS,
        _ => exit::FAILED,
    };
    ExitError::new(code, err.to_string()).into()
}

fn store_err(err: &kevin_store::StoreError) -> anyhow::Error {
    let code = if err.is_unreachable() {
        exit::UNREACHABLE
    } else {
        exit::FAILED
    };
    ExitError::new(code, err.to_string()).into()
}
