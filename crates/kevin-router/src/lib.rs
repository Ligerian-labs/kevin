//! Routing core context (`plan/06-memory-and-learning.md` §Routing).
//!
//! Kevin routes every task to a `(worker, model alias)` pair and learns from
//! the outcome:
//!
//! - [`ModelCatalog`] materialises `[models]` into the routing vocabulary and
//!   hashes it into a `catalog_version`; [`CatalogRepo`] snapshots it into
//!   `routing.model_aliases`.
//! - [`PriceTable`] turns token usage into USD (null when prices are unknown).
//! - [`Router::select`] picks a route with Thompson sampling (or
//!   `epsilon_greedy` / `fixed`) over the learned [`RouteStats`], honouring the
//!   caller's exclusions, the remaining budget and the tier preferred for the
//!   task's complexity.
//! - [`Router::record_outcome`] folds a terminal attempt back into
//!   `routing.route_scores` through the `RouteScore` aggregate and returns the
//!   [`RouteScoreUpdated`] payload the orchestrator publishes as
//!   `routing.score_updated`.
//!
//! Determinism: the router never touches a global RNG — it draws from an
//! injected [`RngSource`], and `SelectRouteQuery::rng_seed` pins a single
//! selection (`plan/11-testing.md` §Determinism rules).
//!
//! ```no_run
//! use std::sync::Arc;
//! use kevin_config::KevinConfig;
//! use kevin_domain::TaskKind;
//! use kevin_router::{InMemoryRouteScoreRepo, Router, SelectRouteQuery};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = KevinConfig::default();
//! let router = Router::from_config(&config, Arc::new(InMemoryRouteScoreRepo::new()));
//! let selection = router.select(SelectRouteQuery::new(TaskKind::Implement)).await?;
//! println!("{} via {}", selection.route, selection.policy);
//! # Ok(()) }
//! ```
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry` and `kevin-store` (persistence only); nothing in the
//! orchestration or interface layers is visible from here.

pub mod catalog;
pub mod error;
pub mod leaderboard;
pub mod pg;
pub mod price;
pub mod rng;
pub mod router;
pub mod score;

pub use catalog::{CatalogEntry, ModelCatalog, catalog_version, tier_quality_prior};
pub use error::RoutingError;
pub use leaderboard::{render_explain, render_leaderboard};
pub use pg::{CatalogRepo, CatalogSync, PgRouteScoreRepo};
pub use price::{AliasPrice, PriceTable, UsageLike};
pub use rng::{FixedSeedRngSource, OsRngSource, RngSource, SeededRngSource, sample_beta};
pub use router::{
    CandidateScore, Policy, RouteSelection, Router, SelectRouteQuery, TAG_BONUS, TIER_BONUS,
};
pub use score::{
    AttemptRef, InMemoryRouteScoreRepo, LeaderboardRow, RouteScoreRepo, RouteScoreUpdated,
    next_stats, reset_stats, sort_leaderboard,
};

// Re-exported for callers that only depend on `kevin-router`.
pub use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome, ResetRouteScore};
pub use kevin_domain::{RouteStats, Tier};
