//! Route selection (`plan/06-memory-and-learning.md` §2.2).
//!
//! [`Router::select`] answers "which `(worker, model alias)` should run this
//! task?" and [`Router::record_outcome`] feeds the result back. Selection is
//! read-only: a `(kind, alias)` pair without a row in `routing.route_scores`
//! simply routes on its tier priors, and the row appears when the first
//! outcome is recorded.

use std::collections::BTreeMap;
use std::sync::Arc;

use kevin_config::{KevinConfig, Role, Roles, Routing, RoutingPolicy};
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome, ResetRouteScore};
use kevin_domain::{
    Complexity, Decimal, ModelAlias, Route, RouteStats, TaskKind, Tier, WorkerKind,
};
use rand::RngExt;
use rand::rngs::StdRng;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};

use crate::catalog::{CatalogEntry, ModelCatalog, domain_tier};
use crate::error::RoutingError;
use crate::price::PriceTable;
use crate::rng::{FixedSeedRngSource, OsRngSource, RngSource, sample_beta};
use crate::score::{
    AttemptRef, LeaderboardRow, RouteScoreRepo, RouteScoreUpdated, sort_leaderboard,
};

/// Score bonus for a candidate in the tier preferred for the task's complexity
/// (`plan/06-memory-and-learning.md` §2.2 step 2). Soft, never a filter.
pub const TIER_BONUS: f32 = 0.10;

/// Score bonus for a candidate carrying every requested tag. Soft, never a
/// filter (`plan/06` lists `tags` on the query without fixing its arithmetic).
pub const TAG_BONUS: f32 = 0.05;

/// Normalised cost/latency used when the statistic is unknown.
pub const UNKNOWN_NORM: f32 = 0.5;

/// How a route was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Thompson sampling over the Beta posteriors.
    Thompson,
    /// Epsilon-greedy on the empirical win rate.
    EpsilonGreedy,
    /// First configured candidate that is not excluded.
    Fixed,
    /// No candidate list applied: `[roles]` (role kinds) or `[roles].default`.
    Fallback,
}

impl Policy {
    /// `snake_case` name, identical to the serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Policy::Thompson => "thompson",
            Policy::EpsilonGreedy => "epsilon_greedy",
            Policy::Fixed => "fixed",
            Policy::Fallback => "fallback",
        }
    }

    const fn from_config(policy: RoutingPolicy) -> Self {
        match policy {
            RoutingPolicy::Thompson => Policy::Thompson,
            RoutingPolicy::EpsilonGreedy => Policy::EpsilonGreedy,
            RoutingPolicy::Fixed => Policy::Fixed,
        }
    }
}

impl std::fmt::Display for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the caller knows about the task to route.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectRouteQuery {
    /// Task kind (drives the candidate list).
    pub kind: TaskKind,
    /// Estimated complexity (tier preference + effort).
    pub complexity: Complexity,
    /// Capability tags the task would like (soft preference).
    pub tags: Vec<String>,
    /// Aliases the caller refuses (retry exclusion).
    pub exclude: Vec<ModelAlias>,
    /// Remaining run budget; candidates whose mean cost exceeds it are dropped.
    pub budget_left_usd: Option<Decimal>,
    /// Pins the RNG of this one selection (tests, `kevin routes explain`).
    pub rng_seed: Option<u64>,
}

impl SelectRouteQuery {
    /// A query for `kind` with medium complexity and no constraints.
    #[must_use]
    pub fn new(kind: TaskKind) -> Self {
        Self {
            kind,
            complexity: Complexity::Medium,
            tags: Vec::new(),
            exclude: Vec::new(),
            budget_left_usd: None,
            rng_seed: None,
        }
    }

    /// Builder: complexity.
    #[must_use]
    pub fn complexity(mut self, complexity: Complexity) -> Self {
        self.complexity = complexity;
        self
    }

    /// Builder: wanted tags.
    #[must_use]
    pub fn tags<I: IntoIterator<Item = S>, S: Into<String>>(mut self, tags: I) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Builder: excluded aliases (retry).
    #[must_use]
    pub fn exclude<I: IntoIterator<Item = ModelAlias>>(mut self, aliases: I) -> Self {
        self.exclude = aliases.into_iter().collect();
        self
    }

    /// Builder: remaining budget.
    #[must_use]
    pub fn budget_left_usd(mut self, budget: Decimal) -> Self {
        self.budget_left_usd = Some(budget);
        self
    }

    /// Builder: RNG seed for this selection.
    #[must_use]
    pub fn rng_seed(mut self, seed: u64) -> Self {
        self.rng_seed = Some(seed);
        self
    }
}

/// One candidate's arithmetic, kept for `task.routed` and `kevin routes explain`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateScore {
    /// The alias.
    pub alias: ModelAlias,
    /// Its tier.
    pub tier: Tier,
    /// Beta sample (`thompson`), win rate (`epsilon_greedy`) or `p_success`.
    pub sampled_success: f32,
    /// `quality_ema`, or the tier's quality prior while unjudged.
    pub quality: f32,
    /// Min-max normalised mean cost over the candidate set (unknown → 0.5).
    pub norm_cost: f32,
    /// Min-max normalised mean wall time (unknown → 0.5).
    pub norm_latency: f32,
    /// Final score (0 for excluded candidates).
    pub score: f32,
    /// Outcomes recorded for this `(kind, alias)` pair.
    pub samples: u32,
    /// Why the candidate was not eligible, when it was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<String>,
    /// Whether this candidate was selected.
    #[serde(default)]
    pub selected: bool,
}

impl CandidateScore {
    /// Whether the candidate took part in the selection.
    #[must_use]
    pub const fn is_eligible(&self) -> bool {
        self.excluded_reason.is_none()
    }
}

/// The router's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteSelection {
    /// The chosen route.
    pub route: Route,
    /// How it was chosen.
    pub policy: Policy,
    /// Every candidate considered, config order.
    pub candidates: Vec<CandidateScore>,
    /// Catalog version the decision was made against.
    pub catalog_version: String,
    /// Whether the exploration floor picked the route.
    #[serde(default)]
    pub explored: bool,
}

impl RouteSelection {
    /// The selected alias.
    #[must_use]
    pub fn alias(&self) -> &ModelAlias {
        &self.route.model
    }

    /// The selected candidate's arithmetic.
    #[must_use]
    pub fn selected(&self) -> Option<&CandidateScore> {
        self.candidates.iter().find(|c| c.selected)
    }
}

/// Model catalog + learned scores + selection policy
/// (`plan/06-memory-and-learning.md` §2).
#[derive(Debug)]
pub struct Router {
    catalog: Arc<ModelCatalog>,
    routing: Routing,
    roles: Roles,
    scores: Arc<dyn RouteScoreRepo>,
    rng: Arc<dyn RngSource>,
}

impl Router {
    /// Builds a router from a resolved configuration and a score repository.
    #[must_use]
    pub fn from_config(config: &KevinConfig, scores: Arc<dyn RouteScoreRepo>) -> Self {
        Self::new(Arc::new(ModelCatalog::from_config(config)), config, scores)
    }

    /// Builds a router around an already materialised catalog.
    #[must_use]
    pub fn new(
        catalog: Arc<ModelCatalog>,
        config: &KevinConfig,
        scores: Arc<dyn RouteScoreRepo>,
    ) -> Self {
        Self {
            catalog,
            routing: config.routing.clone(),
            roles: config.roles.clone(),
            scores,
            rng: Arc::new(OsRngSource),
        }
    }

    /// Builder: injects a deterministic RNG source (tests, `explain`).
    #[must_use]
    pub fn with_rng(mut self, rng: Arc<dyn RngSource>) -> Self {
        self.rng = rng;
        self
    }

    /// The catalog this router routes against.
    #[must_use]
    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    /// The price table of the catalog.
    #[must_use]
    pub fn prices(&self) -> &PriceTable {
        self.catalog.prices()
    }

    /// The score repository.
    #[must_use]
    pub fn scores(&self) -> &Arc<dyn RouteScoreRepo> {
        &self.scores
    }

    /// The `[roles]` binding a role kind resolves to, if `kind` is one
    /// (`understand`/`plan` → planner, `clarify` → clarifier, `evaluate` →
    /// judge, `integrate` → integrator). These kinds bypass routing entirely.
    #[must_use]
    pub const fn role_for_kind(kind: &TaskKind) -> Option<Role> {
        match kind {
            TaskKind::Understand | TaskKind::Plan => Some(Role::Planner),
            TaskKind::Clarify => Some(Role::Clarifier),
            TaskKind::Evaluate => Some(Role::Judge),
            TaskKind::Integrate => Some(Role::Integrator),
            _ => None,
        }
    }

    /// Selects a route (`plan/06-memory-and-learning.md` §2.2).
    pub async fn select(&self, query: SelectRouteQuery) -> Result<RouteSelection, RoutingError> {
        let kind = query.kind.clone();
        let selection = self.select_inner(query).await?;
        metrics::counter!(
            "kevin_router_selections_total",
            "kind" => kind.to_string(),
            "policy" => selection.policy.as_str(),
            "model_alias" => selection.route.model.to_string(),
            "explored" => if selection.explored { "true" } else { "false" },
        )
        .increment(1);
        tracing::debug!(
            event = kevin_telemetry::events::router::SELECTED,
            task_kind = %kind,
            policy = selection.policy.as_str(),
            model_alias = %selection.route.model,
            explored = selection.explored,
            "route selected"
        );
        Ok(selection)
    }

    async fn select_inner(&self, query: SelectRouteQuery) -> Result<RouteSelection, RoutingError> {
        // Role kinds are bound in `[roles]`, never learned (plan/06 §2.1).
        if let Some(role) = Self::role_for_kind(&query.kind) {
            return self.role_selection(role, &query);
        }

        let configured: Vec<ModelAlias> = self
            .routing
            .kinds
            .get(&query.kind)
            .map(|k| k.candidates.clone())
            .unwrap_or_default();
        let (aliases, mut policy) = if configured.is_empty() {
            (vec![self.roles.default.clone()], Policy::Fallback)
        } else {
            (configured, Policy::from_config(self.routing.policy))
        };

        let stats = self.scores.stats_for(&query.kind, &aliases).await?;
        let mut candidates = self.build_candidates(&aliases, &stats, &query);
        if !candidates.iter().any(CandidateScore::is_eligible) && policy != Policy::Fallback {
            // Everything was filtered out → `[roles].default` (plan/06 §2.2 step 1).
            let fallback = vec![self.roles.default.clone()];
            let fallback_stats = self.scores.stats_for(&query.kind, &fallback).await?;
            let fallback_candidates = self.build_candidates(&fallback, &fallback_stats, &query);
            if fallback_candidates.iter().any(CandidateScore::is_eligible) {
                candidates = fallback_candidates;
                policy = Policy::Fallback;
            }
        }
        if !candidates.iter().any(CandidateScore::is_eligible) {
            return Err(RoutingError::NoRoute {
                task_kind: query.kind.clone(),
                reason: reasons(&candidates),
            });
        }

        let mut rng = self.rng_for(&query);
        normalise(&mut candidates, &stats);
        self.score(&mut candidates, &stats, &query, policy, &mut rng);
        let (index, explored) = self.pick(&candidates, policy, &mut rng);
        candidates[index].selected = true;
        let alias = candidates[index].alias.clone();
        let worker = self.worker_of(&alias, "routing.kinds")?;

        Ok(RouteSelection {
            route: Route::new(worker, alias).with_effort(query.complexity.default_effort()),
            policy,
            candidates,
            catalog_version: self.catalog.version().to_owned(),
            explored,
        })
    }

    fn rng_for(&self, query: &SelectRouteQuery) -> StdRng {
        match query.rng_seed {
            Some(seed) => FixedSeedRngSource(seed).rng(),
            None => self.rng.rng(),
        }
    }

    fn role_selection(
        &self,
        role: Role,
        query: &SelectRouteQuery,
    ) -> Result<RouteSelection, RoutingError> {
        let alias = self.roles.alias_for(role).clone();
        let entry = self
            .catalog
            .get(&alias)
            .ok_or_else(|| RoutingError::UnknownAlias {
                alias: alias.clone(),
                referenced_by: format!("roles.{role}"),
            })?;
        let effort = self
            .roles
            .effort
            .get(&role)
            .copied()
            .unwrap_or_else(|| query.complexity.default_effort());
        let candidate = CandidateScore {
            alias: alias.clone(),
            tier: entry.tier,
            sampled_success: 1.0,
            quality: entry.quality_prior(),
            norm_cost: UNKNOWN_NORM,
            norm_latency: UNKNOWN_NORM,
            score: 1.0,
            samples: 0,
            excluded_reason: None,
            selected: true,
        };
        Ok(RouteSelection {
            route: Route::new(entry.worker, alias).with_effort(effort),
            policy: Policy::Fallback,
            candidates: vec![candidate],
            catalog_version: self.catalog.version().to_owned(),
            explored: false,
        })
    }

    fn build_candidates(
        &self,
        aliases: &[ModelAlias],
        stats: &BTreeMap<ModelAlias, RouteStats>,
        query: &SelectRouteQuery,
    ) -> Vec<CandidateScore> {
        aliases
            .iter()
            .map(|alias| {
                let entry = self.catalog.get(alias);
                let stat = stats.get(alias);
                let excluded_reason = if entry.is_none() {
                    Some(format!("unknown alias `{alias}` (not in [models])"))
                } else if !entry.is_some_and(|e| e.enabled) {
                    entry.map(|e| format!("worker `{}` is disabled", e.worker))
                } else if query.exclude.contains(alias) {
                    Some("excluded by the caller (retry exclusion)".to_owned())
                } else {
                    over_budget(stat, query.budget_left_usd)
                };
                CandidateScore {
                    alias: alias.clone(),
                    tier: entry.map_or(domain_tier(kevin_config::Tier::Balanced), |e| e.tier),
                    sampled_success: 0.0,
                    quality: stat
                        .and_then(|s| s.quality_ema)
                        .unwrap_or_else(|| entry.map_or(0.5, CatalogEntry::quality_prior)),
                    norm_cost: UNKNOWN_NORM,
                    norm_latency: UNKNOWN_NORM,
                    score: 0.0,
                    samples: stat.map_or(0, |s| s.attempts),
                    excluded_reason,
                    selected: false,
                }
            })
            .collect()
    }

    fn score(
        &self,
        candidates: &mut [CandidateScore],
        stats: &BTreeMap<ModelAlias, RouteStats>,
        query: &SelectRouteQuery,
        policy: Policy,
        rng: &mut StdRng,
    ) {
        let preferred = self.preferred_tier(query.complexity);
        let (qw, cw, lw) = (
            to_f32(self.routing.quality_weight),
            to_f32(self.routing.cost_weight),
            to_f32(self.routing.latency_weight),
        );
        for candidate in candidates.iter_mut() {
            if !candidate.is_eligible() {
                continue;
            }
            let stat = stats.get(&candidate.alias);
            let prior = self.catalog.prior_for(&candidate.alias);
            candidate.sampled_success = sampled_success(stat, prior, policy, rng);
            let base = qw * candidate.quality
                + cw * (1.0 - candidate.norm_cost)
                + lw * (1.0 - candidate.norm_latency);
            candidate.score = candidate.sampled_success * base;
            if candidate.tier == preferred {
                candidate.score += TIER_BONUS;
            }
            if !query.tags.is_empty() && self.has_all_tags(&candidate.alias, &query.tags) {
                candidate.score += TAG_BONUS;
            }
        }
    }

    fn preferred_tier(&self, complexity: Complexity) -> Tier {
        let prefer = &self.routing.prefer_tier_for_complexity;
        domain_tier(match complexity {
            Complexity::Low => prefer.low,
            Complexity::Medium => prefer.medium,
            Complexity::High => prefer.high,
        })
    }

    fn has_all_tags(&self, alias: &ModelAlias, tags: &[String]) -> bool {
        self.catalog
            .get(alias)
            .is_some_and(|entry| tags.iter().all(|t| entry.has_tag(t)))
    }

    /// Picks a candidate: the exploration floor first (uniform among
    /// under-sampled candidates with probability `routing.exploration`), then
    /// argmax of the score. `fixed`/`fallback` never explore.
    fn pick(
        &self,
        candidates: &[CandidateScore],
        policy: Policy,
        rng: &mut StdRng,
    ) -> (usize, bool) {
        let eligible: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_eligible())
            .map(|(i, _)| i)
            .collect();
        if matches!(policy, Policy::Fixed | Policy::Fallback) || eligible.len() == 1 {
            return (eligible[0], false);
        }
        let under_sampled: Vec<usize> = eligible
            .iter()
            .copied()
            .filter(|&i| candidates[i].samples < self.routing.min_samples_before_exploit)
            .collect();
        if !under_sampled.is_empty() && rng.random_bool(self.routing.exploration.clamp(0.0, 1.0)) {
            let pick = rng.random_range(0..under_sampled.len());
            return (under_sampled[pick], true);
        }
        let best = eligible
            .iter()
            .copied()
            .reduce(|a, b| {
                if candidates[b].score > candidates[a].score {
                    b
                } else {
                    a
                }
            })
            .unwrap_or(eligible[0]);
        (best, false)
    }

    fn worker_of(
        &self,
        alias: &ModelAlias,
        referenced_by: &str,
    ) -> Result<WorkerKind, RoutingError> {
        self.catalog
            .get(alias)
            .map(|entry| entry.worker)
            .ok_or_else(|| RoutingError::UnknownAlias {
                alias: alias.clone(),
                referenced_by: referenced_by.to_owned(),
            })
    }

    /// Records a terminal outcome (`plan/06-memory-and-learning.md` §2.4).
    ///
    /// The cold-start prior comes from the alias' tier, so callers never have
    /// to fill `RecordRouteOutcome::prior` themselves.
    pub async fn record_outcome(
        &self,
        cmd: RecordRouteOutcome,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError> {
        self.record_attempt_outcome(cmd, None).await
    }

    /// Records a terminal outcome that belongs to a task attempt; recording the
    /// same `attempt_id` twice leaves the statistics untouched.
    pub async fn record_attempt_outcome(
        &self,
        mut cmd: RecordRouteOutcome,
        attempt: Option<AttemptRef>,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError> {
        cmd.prior = self.prior_for(&cmd.alias);
        let updated = self
            .scores
            .record(&cmd, attempt, self.catalog.version())
            .await?;
        if updated.is_some() {
            metrics::counter!(
                "kevin_router_outcomes_total",
                "kind" => cmd.task_kind.to_string(),
                "model_alias" => cmd.alias.to_string(),
                "success" => if cmd.success { "true" } else { "false" },
            )
            .increment(1);
            tracing::debug!(
                event = kevin_telemetry::events::router::SCORE_UPDATED,
                task_kind = %cmd.task_kind,
                model_alias = %cmd.alias,
                success = cmd.success,
                "route score updated"
            );
        }
        Ok(updated)
    }

    /// The cold-start prior of an alias (its tier prior, `plan/06` §2.3).
    #[must_use]
    pub fn prior_for(&self, alias: &ModelAlias) -> BetaPrior {
        self.catalog.prior_for(alias)
    }

    /// The leaderboard rows, optionally for one kind.
    pub async fn leaderboard(
        &self,
        kind: Option<&TaskKind>,
    ) -> Result<Vec<LeaderboardRow>, RoutingError> {
        let mut rows = self.scores.leaderboard(kind).await?;
        sort_leaderboard(&mut rows);
        Ok(rows)
    }

    /// Resets learned scores back to their tier priors (`kevin routes reset`).
    /// `None` filters mean "everything".
    pub async fn reset(
        &self,
        kind: Option<&TaskKind>,
        alias: Option<&ModelAlias>,
    ) -> Result<Vec<RouteScoreUpdated>, RoutingError> {
        let rows = self.scores.leaderboard(kind).await?;
        let mut updates = Vec::new();
        for row in rows {
            if alias.is_some_and(|a| a != &row.alias) {
                continue;
            }
            let reset = ResetRouteScore {
                task_kind: row.task_kind.clone(),
                alias: row.alias.clone(),
                prior: self.prior_for(&row.alias),
            };
            if let Some(update) = self.scores.reset(&reset).await? {
                updates.push(update);
            }
        }
        Ok(updates)
    }
}

/// Min-max normalises mean cost and mean wall time over the eligible set
/// (unknown values stay at [`UNKNOWN_NORM`]).
fn normalise(candidates: &mut [CandidateScore], stats: &BTreeMap<ModelAlias, RouteStats>) {
    let values = |f: fn(&RouteStats) -> Option<f64>| -> Vec<Option<f64>> {
        candidates
            .iter()
            .map(|c| {
                if c.is_eligible() {
                    stats.get(&c.alias).and_then(f)
                } else {
                    None
                }
            })
            .collect()
    };
    let costs = values(mean_cost_f64);
    let walls = values(mean_wall_f64);
    let (cost_lo, cost_hi) = range(&costs);
    let (wall_lo, wall_hi) = range(&walls);
    for (index, candidate) in candidates.iter_mut().enumerate() {
        candidate.norm_cost = min_max(costs[index], cost_lo, cost_hi);
        candidate.norm_latency = min_max(walls[index], wall_lo, wall_hi);
    }
}

fn range(values: &[Option<f64>]) -> (Option<f64>, Option<f64>) {
    let known: Vec<f64> = values.iter().filter_map(|v| *v).collect();
    (
        known.iter().copied().reduce(f64::min),
        known.iter().copied().reduce(f64::max),
    )
}

fn min_max(value: Option<f64>, min: Option<f64>, max: Option<f64>) -> f32 {
    match (value, min, max) {
        (Some(v), Some(lo), Some(hi)) if (hi - lo).abs() > f64::EPSILON => {
            to_f32(((v - lo) / (hi - lo)).clamp(0.0, 1.0))
        }
        _ => UNKNOWN_NORM,
    }
}

fn mean_cost_f64(stats: &RouteStats) -> Option<f64> {
    stats.mean_cost_usd().and_then(|d| d.to_f64())
}

#[allow(clippy::cast_precision_loss)]
fn mean_wall_f64(stats: &RouteStats) -> Option<f64> {
    stats.mean_wall_ms().map(|ms| ms as f64)
}

/// Draws the "probability of success" term of the score for one candidate.
fn sampled_success(
    stats: Option<&RouteStats>,
    prior: BetaPrior,
    policy: Policy,
    rng: &mut StdRng,
) -> f32 {
    let (alpha, beta) = stats.map_or((prior.alpha, prior.beta), |s| (s.alpha, s.beta));
    match policy {
        Policy::Thompson => sample_beta(rng, alpha, beta),
        Policy::EpsilonGreedy => stats
            .filter(|s| s.attempts > 0)
            .map_or(alpha / (alpha + beta), RouteStats::win_rate),
        Policy::Fixed | Policy::Fallback => alpha / (alpha + beta),
    }
}

fn over_budget(stats: Option<&RouteStats>, budget_left: Option<Decimal>) -> Option<String> {
    let (stats, budget) = (stats?, budget_left?);
    let mean = stats.mean_cost_usd()?;
    (mean > budget).then(|| format!("mean cost ${mean} exceeds the remaining budget ${budget}"))
}

fn reasons(candidates: &[CandidateScore]) -> String {
    if candidates.is_empty() {
        return "no candidates configured".to_owned();
    }
    candidates
        .iter()
        .map(|c| {
            format!(
                "{}: {}",
                c.alias,
                c.excluded_reason.as_deref().unwrap_or("eligible")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[allow(clippy::cast_possible_truncation)]
fn to_f32(value: f64) -> f32 {
    value as f32
}
