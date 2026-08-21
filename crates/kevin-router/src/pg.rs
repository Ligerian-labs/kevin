//! Postgres persistence for the `routing` schema
//! (`crates/kevin-store/migrations/0003_routing.sql`).
//!
//! - [`CatalogRepo`] snapshots `[models]` into `routing.model_aliases`, keyed by
//!   the catalog version, so a leaderboard row always names a catalog that can
//!   still be read back.
//! - [`PgRouteScoreRepo`] stores `RouteScore` state in `routing.route_scores`
//!   and appends every terminal attempt to `routing.route_outcomes`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use kevin_domain::route_score::{RecordRouteOutcome, ResetRouteScore};
use kevin_domain::{Decimal, FailureClass, ModelAlias, RouteStats, TaskKind};
use kevin_store::{PgPool, StoreError};
use sqlx::Row as _;
use uuid::Uuid;

use crate::catalog::ModelCatalog;
use crate::error::RoutingError;
use crate::score::{
    AttemptRef, LeaderboardRow, RouteScoreRepo, RouteScoreUpdated, next_stats, reset_stats,
    sort_leaderboard,
};

/// Result of a catalog snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSync {
    /// The version that was synced.
    pub catalog_version: String,
    /// Aliases in the catalog.
    pub aliases: usize,
    /// Rows written (inserted or refreshed).
    pub rows_written: usize,
    /// Whether the version was already present before this call.
    pub already_present: bool,
}

/// Access to `routing.model_aliases`.
#[derive(Debug, Clone)]
pub struct CatalogRepo {
    pool: PgPool,
}

impl CatalogRepo {
    /// Wraps a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Writes the catalog snapshot for its version (idempotent).
    pub async fn sync(&self, catalog: &ModelCatalog) -> Result<CatalogSync, RoutingError> {
        let version = catalog.version();
        let existing: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM routing.model_aliases WHERE catalog_version = $1",
        )
        .bind(version)
        .fetch_one(&self.pool)
        .await?;

        let mut written = 0usize;
        let mut tx = self.pool.begin().await?;
        for entry in catalog.entries() {
            let extra = serde_json::to_value(&entry.extra).map_err(StoreError::from)?;
            let context_tokens = entry.context_tokens.and_then(|n| i64::try_from(n).ok());
            let done = sqlx::query(
                "INSERT INTO routing.model_aliases \
                 (catalog_version, alias, worker, model, tier, context_tokens, \
                  input_usd_per_m, output_usd_per_m, tags, extra) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (catalog_version, alias) DO UPDATE SET \
                 worker = EXCLUDED.worker, model = EXCLUDED.model, tier = EXCLUDED.tier, \
                 context_tokens = EXCLUDED.context_tokens, \
                 input_usd_per_m = EXCLUDED.input_usd_per_m, \
                 output_usd_per_m = EXCLUDED.output_usd_per_m, \
                 tags = EXCLUDED.tags, extra = EXCLUDED.extra",
            )
            .bind(version)
            .bind(entry.alias.as_str())
            .bind(entry.worker.as_str())
            .bind(&entry.model)
            .bind(entry.tier.as_str())
            .bind(context_tokens)
            .bind(entry.input_usd_per_m)
            .bind(entry.output_usd_per_m)
            .bind(&entry.tags)
            .bind(&extra)
            .execute(&mut *tx)
            .await?;
            written += usize::try_from(done.rows_affected()).unwrap_or(0);
        }
        tx.commit().await?;

        Ok(CatalogSync {
            catalog_version: version.to_owned(),
            aliases: catalog.len(),
            rows_written: written,
            already_present: existing > 0,
        })
    }

    /// Aliases stored for `catalog_version`, alias order.
    pub async fn aliases_of(&self, catalog_version: &str) -> Result<Vec<String>, RoutingError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT alias FROM routing.model_aliases WHERE catalog_version = $1 ORDER BY alias",
        )
        .bind(catalog_version)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(alias,)| alias).collect())
    }

    /// Every catalog version stored, newest first.
    pub async fn versions(&self) -> Result<Vec<String>, RoutingError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT catalog_version FROM routing.model_aliases \
             GROUP BY catalog_version ORDER BY min(first_seen) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(version,)| version).collect())
    }
}

/// `RouteScoreRepo` backed by `routing.route_scores` / `routing.route_outcomes`.
#[derive(Debug, Clone)]
pub struct PgRouteScoreRepo {
    pool: PgPool,
}

impl PgRouteScoreRepo {
    /// Wraps a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The pool this repository writes to.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl RouteScoreRepo for PgRouteScoreRepo {
    async fn stats_for(
        &self,
        task_kind: &TaskKind,
        aliases: &[ModelAlias],
    ) -> Result<BTreeMap<ModelAlias, RouteStats>, RoutingError> {
        if aliases.is_empty() {
            return Ok(BTreeMap::new());
        }
        let names: Vec<String> = aliases.iter().map(ToString::to_string).collect();
        let rows = sqlx::query(
            "SELECT task_kind, alias, attempts, successes, alpha, beta, quality_ema, sum_quality, quality_samples, sum_cost_usd, cost_samples, sum_wall_ms, last_used, version \
             FROM routing.route_scores WHERE task_kind = $1 AND alias = ANY($2)",
        )
        .bind(task_kind.to_string())
        .bind(&names)
        .fetch_all(&self.pool)
        .await?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (_, alias, stats, _) = decode_score(&row)?;
            out.insert(alias, stats);
        }
        Ok(out)
    }

    async fn record(
        &self,
        outcome: &RecordRouteOutcome,
        attempt: Option<AttemptRef>,
        catalog_version: &str,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError> {
        let mut tx = self.pool.begin().await?;
        let kind = outcome.task_kind.to_string();
        let alias = outcome.alias.to_string();
        let wall_ms = i64::try_from(outcome.wall_ms).unwrap_or(i64::MAX);

        // The outcome row is keyed by attempt: replaying an attempt refreshes
        // the row but never re-applies the statistics (plan/06 §3.3).
        let fresh: Option<(Uuid,)> = sqlx::query_as(
            "INSERT INTO routing.route_outcomes \
             (id, run_id, task_id, attempt_id, task_kind, alias, catalog_version, \
              success, quality, cost_usd, wall_ms, failure_class, recorded_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
             ON CONFLICT (attempt_id) WHERE attempt_id IS NOT NULL DO NOTHING \
             RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(attempt.map(|a| a.run_id))
        .bind(attempt.map(|a| a.task_id))
        .bind(attempt.map(|a| a.attempt_id))
        .bind(&kind)
        .bind(&alias)
        .bind(catalog_version)
        .bind(outcome.success)
        .bind(outcome.quality)
        .bind(outcome.cost_usd)
        .bind(wall_ms)
        .bind(outcome.failure_class.map(FailureClass::as_str))
        .bind(outcome.recorded_at)
        .fetch_optional(&mut *tx)
        .await?;

        if fresh.is_none() {
            // Already recorded: refresh the row (re-evaluation may carry a new
            // quality) and leave `route_scores` alone.
            if let Some(attempt) = attempt {
                sqlx::query(
                    "UPDATE routing.route_outcomes SET success = $2, quality = $3, \
                     cost_usd = $4, wall_ms = $5, failure_class = $6, \
                     catalog_version = $7 WHERE attempt_id = $1",
                )
                .bind(attempt.attempt_id)
                .bind(outcome.success)
                .bind(outcome.quality)
                .bind(outcome.cost_usd)
                .bind(wall_ms)
                .bind(outcome.failure_class.map(FailureClass::as_str))
                .bind(catalog_version)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(None);
        }

        let current = load_for_update(&mut tx, &kind, &alias).await?;
        let stats = next_stats(current.as_ref(), outcome)?;
        let version = upsert_score(&mut tx, &kind, &alias, &stats).await?;
        tx.commit().await?;

        Ok(Some(RouteScoreUpdated {
            task_kind: outcome.task_kind.clone(),
            alias: outcome.alias.clone(),
            stats,
            success: Some(outcome.success),
            reset: false,
            version,
        }))
    }

    async fn reset(
        &self,
        reset: &ResetRouteScore,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError> {
        let mut tx = self.pool.begin().await?;
        let kind = reset.task_kind.to_string();
        let alias = reset.alias.to_string();
        let Some(current) = load_for_update(&mut tx, &kind, &alias).await? else {
            tx.rollback().await?;
            return Ok(None);
        };
        let stats = reset_stats(Some(&current), reset)?;
        let version = upsert_score(&mut tx, &kind, &alias, &stats).await?;
        tx.commit().await?;
        Ok(Some(RouteScoreUpdated {
            task_kind: reset.task_kind.clone(),
            alias: reset.alias.clone(),
            stats,
            success: None,
            reset: true,
            version,
        }))
    }

    async fn leaderboard(
        &self,
        task_kind: Option<&TaskKind>,
    ) -> Result<Vec<LeaderboardRow>, RoutingError> {
        let rows = match task_kind {
            Some(kind) => {
                sqlx::query(
                    "SELECT task_kind, alias, attempts, successes, alpha, beta, quality_ema, sum_quality, quality_samples, sum_cost_usd, cost_samples, sum_wall_ms, last_used, version \
                     FROM routing.route_leaderboard WHERE task_kind = $1",
                )
                .bind(kind.to_string())
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT task_kind, alias, attempts, successes, alpha, beta, quality_ema, sum_quality, quality_samples, sum_cost_usd, cost_samples, sum_wall_ms, last_used, version \
                     FROM routing.route_leaderboard",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let (task_kind, alias, stats, _) = decode_score(&row)?;
            out.push(LeaderboardRow {
                task_kind,
                alias,
                stats,
            });
        }
        sort_leaderboard(&mut out);
        Ok(out)
    }
}

async fn load_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_kind: &str,
    alias: &str,
) -> Result<Option<RouteStats>, RoutingError> {
    let row = sqlx::query(
        "SELECT task_kind, alias, attempts, successes, alpha, beta, quality_ema, sum_quality, quality_samples, sum_cost_usd, cost_samples, sum_wall_ms, last_used, version \
         FROM routing.route_scores WHERE task_kind = $1 AND alias = $2 FOR UPDATE",
    )
    .bind(task_kind)
    .bind(alias)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|row| decode_score(&row).map(|(_, _, stats, _)| stats))
        .transpose()
}

async fn upsert_score(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_kind: &str,
    alias: &str,
    stats: &RouteStats,
) -> Result<u64, RoutingError> {
    let version: i64 = sqlx::query_scalar(
        "INSERT INTO routing.route_scores \
         (task_kind, alias, attempts, successes, alpha, beta, quality_ema, sum_quality, \
          quality_samples, sum_cost_usd, cost_samples, sum_wall_ms, last_used, version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 1) \
         ON CONFLICT (task_kind, alias) DO UPDATE SET \
         attempts = EXCLUDED.attempts, successes = EXCLUDED.successes, \
         alpha = EXCLUDED.alpha, beta = EXCLUDED.beta, \
         quality_ema = EXCLUDED.quality_ema, sum_quality = EXCLUDED.sum_quality, \
         quality_samples = EXCLUDED.quality_samples, sum_cost_usd = EXCLUDED.sum_cost_usd, \
         cost_samples = EXCLUDED.cost_samples, sum_wall_ms = EXCLUDED.sum_wall_ms, \
         last_used = EXCLUDED.last_used, version = routing.route_scores.version + 1 \
         RETURNING version",
    )
    .bind(task_kind)
    .bind(alias)
    .bind(to_i32(stats.attempts))
    .bind(to_i32(stats.successes))
    .bind(stats.alpha)
    .bind(stats.beta)
    .bind(stats.quality_ema)
    .bind(stats.sum_quality)
    .bind(to_i32(stats.quality_samples))
    .bind(stats.sum_cost_usd)
    .bind(to_i32(stats.cost_samples))
    .bind(i64::try_from(stats.sum_wall_ms).unwrap_or(i64::MAX))
    .bind(stats.last_used)
    .fetch_one(&mut **tx)
    .await?;
    Ok(u64::try_from(version).unwrap_or(0))
}

/// Decodes a `routing.route_scores` / `routing.route_leaderboard` row.
fn decode_score(
    row: &sqlx::postgres::PgRow,
) -> Result<(TaskKind, ModelAlias, RouteStats, u64), RoutingError> {
    let kind_text: String = row.try_get("task_kind")?;
    let alias_text: String = row.try_get("alias")?;
    let task_kind = kind_text.parse::<TaskKind>().map_err(|e| corrupt(&e))?;
    let alias = ModelAlias::new(alias_text).map_err(|e| corrupt(&e))?;
    let sum_wall_ms: i64 = row.try_get("sum_wall_ms")?;
    let version: i64 = row.try_get("version")?;
    let stats = RouteStats {
        attempts: to_u32(row.try_get("attempts")?),
        successes: to_u32(row.try_get("successes")?),
        sum_quality: row.try_get("sum_quality")?,
        quality_samples: to_u32(row.try_get("quality_samples")?),
        sum_cost_usd: row.try_get::<Decimal, _>("sum_cost_usd")?,
        cost_samples: to_u32(row.try_get("cost_samples")?),
        sum_wall_ms: u64::try_from(sum_wall_ms).unwrap_or(0),
        alpha: row.try_get("alpha")?,
        beta: row.try_get("beta")?,
        quality_ema: row.try_get("quality_ema")?,
        last_used: row.try_get("last_used")?,
    };
    Ok((task_kind, alias, stats, u64::try_from(version).unwrap_or(0)))
}

fn corrupt(err: &impl std::fmt::Display) -> RoutingError {
    RoutingError::Store(StoreError::Corrupt {
        table: "routing.route_scores",
        message: err.to_string(),
    })
}

#[allow(clippy::cast_possible_wrap)]
const fn to_i32(value: u32) -> i32 {
    if value > i32::MAX as u32 {
        i32::MAX
    } else {
        value as i32
    }
}

#[allow(clippy::cast_sign_loss)]
const fn to_u32(value: i32) -> u32 {
    if value < 0 { 0 } else { value as u32 }
}
