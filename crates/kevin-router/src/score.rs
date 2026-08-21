//! Route-score persistence (`plan/06-memory-and-learning.md` §2.3–2.4).
//!
//! The *rules* live in the domain: [`RouteScore`] handles
//! [`RecordRouteOutcome`]/[`ResetRouteScore`] and produces the new
//! [`RouteStats`]. This module only decides *where* those statistics live —
//! [`InMemoryRouteScoreRepo`] for unit tests and ephemeral runs,
//! [`PgRouteScoreRepo`](crate::pg::PgRouteScoreRepo) for `routing.route_scores`
//! / `routing.route_outcomes` — and carries the [`RouteScoreUpdated`] DTO the
//! orchestrator wraps into a `routing.score_updated` event.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use async_trait::async_trait;
use kevin_domain::route_score::{RecordRouteOutcome, ResetRouteScore};
use kevin_domain::{
    Aggregate, ModelAlias, RouteScore, RouteScoreCommand, RouteScoreEvent, RouteStats, TaskKind,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::RoutingError;

/// Which attempt an outcome came from — the provenance columns of
/// `routing.route_outcomes` and the idempotency key of the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRef {
    /// Run the attempt belonged to.
    pub run_id: Uuid,
    /// Task the attempt belonged to.
    pub task_id: Uuid,
    /// The attempt itself; recording the same attempt twice is a no-op.
    pub attempt_id: Uuid,
}

impl AttemptRef {
    /// Builds a reference from the three ids.
    #[must_use]
    pub const fn new(run_id: Uuid, task_id: Uuid, attempt_id: Uuid) -> Self {
        Self {
            run_id,
            task_id,
            attempt_id,
        }
    }
}

/// The `routing.score_updated` payload plus the row version that produced it.
///
/// Wrapping this in an `EventEnvelope` and appending it to the event store is
/// the orchestrator's/evaluator's job (`plan/02` §Event catalog); the router
/// only computes and persists the statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteScoreUpdated {
    /// Task kind.
    pub task_kind: TaskKind,
    /// Model alias.
    pub alias: ModelAlias,
    /// Statistics after the update.
    pub stats: RouteStats,
    /// Whether the recorded outcome succeeded (`None` for a reset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// Whether the update reset the score back to its prior.
    #[serde(default)]
    pub reset: bool,
    /// `routing.route_scores.version` after the update (optimistic concurrency).
    #[serde(default)]
    pub version: u64,
}

impl RouteScoreUpdated {
    /// Event type this DTO is the payload of.
    pub const EVENT_TYPE: &'static str = "routing.score_updated";
    /// Aggregate type of the stream the event belongs to.
    pub const AGGREGATE_TYPE: &'static str = kevin_domain::route_score::ROUTE_SCORE_AGGREGATE_TYPE;

    /// The domain event to append to the `route_score` stream.
    #[must_use]
    pub fn to_event(&self) -> RouteScoreEvent {
        RouteScoreEvent::ScoreUpdated {
            task_kind: self.task_kind.clone(),
            alias: self.alias.clone(),
            stats: self.stats.clone(),
            success: self.success,
            reset: self.reset,
        }
    }

    /// Stream id of the `(task_kind, alias)` pair.
    #[must_use]
    pub fn stream_id(&self) -> Uuid {
        RouteScore::id_for(&self.task_kind, &self.alias)
    }

    /// Rebuilds the DTO from a stored `routing.score_updated` event.
    #[must_use]
    pub fn from_event(event: &RouteScoreEvent, version: u64) -> Self {
        let RouteScoreEvent::ScoreUpdated {
            task_kind,
            alias,
            stats,
            success,
            reset,
        } = event;
        Self {
            task_kind: task_kind.clone(),
            alias: alias.clone(),
            stats: stats.clone(),
            success: *success,
            reset: *reset,
            version,
        }
    }
}

/// One row of the route leaderboard (`routing.route_leaderboard`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LeaderboardRow {
    /// Task kind.
    pub task_kind: TaskKind,
    /// Model alias.
    pub alias: ModelAlias,
    /// Statistics of the pair.
    pub stats: RouteStats,
}

/// Applies a route-score command through the domain aggregate, starting from
/// `current` (`None` = the pair has no row yet).
///
/// This is the only place statistics are computed, so the in-memory and the
/// Postgres repository can never disagree with the aggregate.
pub fn apply_command(
    current: Option<&RouteStats>,
    task_kind: &TaskKind,
    alias: &ModelAlias,
    cmd: &RouteScoreCommand,
) -> Result<RouteScoreEvent, RoutingError> {
    let mut aggregate = RouteScore::default();
    if let Some(stats) = current {
        aggregate.apply(&RouteScoreEvent::ScoreUpdated {
            task_kind: task_kind.clone(),
            alias: alias.clone(),
            stats: stats.clone(),
            success: None,
            reset: false,
        });
    }
    let mut events = aggregate.handle(cmd)?;
    Ok(events.remove(0))
}

/// Statistics after applying `outcome` to `current`.
pub fn next_stats(
    current: Option<&RouteStats>,
    outcome: &RecordRouteOutcome,
) -> Result<RouteStats, RoutingError> {
    let event = apply_command(
        current,
        &outcome.task_kind,
        &outcome.alias,
        &RouteScoreCommand::RecordOutcome(outcome.clone()),
    )?;
    let RouteScoreEvent::ScoreUpdated { stats, .. } = event;
    Ok(stats)
}

/// Statistics after resetting `current` to a prior.
pub fn reset_stats(
    current: Option<&RouteStats>,
    reset: &ResetRouteScore,
) -> Result<RouteStats, RoutingError> {
    let event = apply_command(
        current,
        &reset.task_kind,
        &reset.alias,
        &RouteScoreCommand::Reset(reset.clone()),
    )?;
    let RouteScoreEvent::ScoreUpdated { stats, .. } = event;
    Ok(stats)
}

/// Sorts leaderboard rows the way `kevin routes` prints them: kind, then
/// `p_success` desc, then attempts desc, then alias.
pub fn sort_leaderboard(rows: &mut [LeaderboardRow]) {
    rows.sort_by(|a, b| {
        a.task_kind
            .to_string()
            .cmp(&b.task_kind.to_string())
            .then_with(|| {
                b.stats
                    .p_success()
                    .partial_cmp(&a.stats.p_success())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.stats.attempts.cmp(&a.stats.attempts))
            .then_with(|| a.alias.cmp(&b.alias))
    });
}

/// Persistence of `RouteScore` state and of the outcome log.
#[async_trait]
pub trait RouteScoreRepo: Send + Sync + std::fmt::Debug {
    /// Statistics of every requested alias that already has a row.
    async fn stats_for(
        &self,
        task_kind: &TaskKind,
        aliases: &[ModelAlias],
    ) -> Result<BTreeMap<ModelAlias, RouteStats>, RoutingError>;

    /// Records an outcome (append the outcome row, update the score) and
    /// returns the new statistics — or `None` when `attempt` was already
    /// recorded (idempotent replay, `plan/06` §3.3).
    async fn record(
        &self,
        outcome: &RecordRouteOutcome,
        attempt: Option<AttemptRef>,
        catalog_version: &str,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError>;

    /// Resets one pair back to its prior; `None` when no row exists.
    async fn reset(
        &self,
        reset: &ResetRouteScore,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError>;

    /// Leaderboard rows, optionally restricted to one kind, best first.
    async fn leaderboard(
        &self,
        task_kind: Option<&TaskKind>,
    ) -> Result<Vec<LeaderboardRow>, RoutingError>;
}

// ---------------------------------------------------------------------------
// In-memory repository
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MemState {
    scores: BTreeMap<(TaskKind, ModelAlias), (RouteStats, u64)>,
    attempts: BTreeSet<Uuid>,
}

/// In-memory [`RouteScoreRepo`] for unit tests and ephemeral runs.
#[derive(Debug, Default)]
pub struct InMemoryRouteScoreRepo {
    state: Mutex<MemState>,
}

impl InMemoryRouteScoreRepo {
    /// An empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Statistics of one pair, if any.
    #[must_use]
    pub fn get(&self, task_kind: &TaskKind, alias: &ModelAlias) -> Option<RouteStats> {
        self.lock()
            .scores
            .get(&(task_kind.clone(), alias.clone()))
            .map(|(stats, _)| stats.clone())
    }

    /// Number of `(kind, alias)` rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().scores.len()
    }

    /// Whether nothing was recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl RouteScoreRepo for InMemoryRouteScoreRepo {
    async fn stats_for(
        &self,
        task_kind: &TaskKind,
        aliases: &[ModelAlias],
    ) -> Result<BTreeMap<ModelAlias, RouteStats>, RoutingError> {
        let state = self.lock();
        Ok(aliases
            .iter()
            .filter_map(|alias| {
                state
                    .scores
                    .get(&(task_kind.clone(), alias.clone()))
                    .map(|(stats, _)| (alias.clone(), stats.clone()))
            })
            .collect())
    }

    async fn record(
        &self,
        outcome: &RecordRouteOutcome,
        attempt: Option<AttemptRef>,
        _catalog_version: &str,
    ) -> Result<Option<RouteScoreUpdated>, RoutingError> {
        let mut state = self.lock();
        if let Some(attempt) = attempt
            && !state.attempts.insert(attempt.attempt_id)
        {
            return Ok(None);
        }
        let key = (outcome.task_kind.clone(), outcome.alias.clone());
        let current = state.scores.get(&key).map(|(stats, _)| stats.clone());
        let stats = next_stats(current.as_ref(), outcome)?;
        let version = state.scores.get(&key).map_or(0, |(_, v)| *v) + 1;
        state.scores.insert(key, (stats.clone(), version));
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
        let mut state = self.lock();
        let key = (reset.task_kind.clone(), reset.alias.clone());
        let Some((current, version)) = state.scores.get(&key).cloned() else {
            return Ok(None);
        };
        let stats = reset_stats(Some(&current), reset)?;
        let version = version + 1;
        state.scores.insert(key, (stats.clone(), version));
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
        let state = self.lock();
        let mut rows: Vec<LeaderboardRow> = state
            .scores
            .iter()
            .filter(|((kind, _), _)| task_kind.is_none_or(|wanted| wanted == kind))
            .map(|((kind, alias), (stats, _))| LeaderboardRow {
                task_kind: kind.clone(),
                alias: alias.clone(),
                stats: stats.clone(),
            })
            .collect();
        sort_leaderboard(&mut rows);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kevin_domain::route_score::BetaPrior;
    use kevin_domain::{Decimal, FailureClass};

    use super::*;

    fn alias(name: &str) -> ModelAlias {
        ModelAlias::new(name).unwrap()
    }

    fn outcome(success: bool) -> RecordRouteOutcome {
        RecordRouteOutcome {
            task_kind: TaskKind::Implement,
            alias: alias("sonnet5-claude"),
            success,
            quality: None,
            cost_usd: None,
            wall_ms: 0,
            failure_class: (!success).then_some(FailureClass::Permanent),
            recorded_at: Utc::now(),
            prior: BetaPrior::for_tier(kevin_domain::Tier::Balanced),
        }
    }

    #[tokio::test]
    async fn record_seeds_the_prior_then_updates_it() {
        let repo = InMemoryRouteScoreRepo::new();
        let updated = repo
            .record(&outcome(true), None, "v1")
            .await
            .unwrap()
            .expect("recorded");
        assert_eq!((updated.stats.alpha, updated.stats.beta), (3.0, 1.0));
        assert_eq!(updated.version, 1);

        let updated = repo
            .record(&outcome(false), None, "v1")
            .await
            .unwrap()
            .expect("recorded");
        assert_eq!((updated.stats.alpha, updated.stats.beta), (3.0, 2.0));
        assert_eq!(updated.stats.attempts, 2);
        assert_eq!(updated.version, 2);
    }

    #[tokio::test]
    async fn recording_the_same_attempt_twice_is_a_no_op() {
        let repo = InMemoryRouteScoreRepo::new();
        let attempt = AttemptRef::new(Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        assert!(
            repo.record(&outcome(true), Some(attempt), "v1")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            repo.record(&outcome(true), Some(attempt), "v1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            repo.get(&TaskKind::Implement, &alias("sonnet5-claude"))
                .unwrap()
                .attempts,
            1
        );
    }

    #[tokio::test]
    async fn reset_restores_the_prior_and_keeps_last_used() {
        let repo = InMemoryRouteScoreRepo::new();
        repo.record(&outcome(true), None, "v1").await.unwrap();
        let reset = ResetRouteScore {
            task_kind: TaskKind::Implement,
            alias: alias("sonnet5-claude"),
            prior: BetaPrior::for_tier(kevin_domain::Tier::Frontier),
        };
        let updated = repo.reset(&reset).await.unwrap().expect("row exists");
        assert!(updated.reset);
        assert_eq!((updated.stats.alpha, updated.stats.beta), (3.0, 1.0));
        assert_eq!(updated.stats.attempts, 0);
        assert!(updated.stats.last_used.is_some());

        let missing = ResetRouteScore {
            task_kind: TaskKind::Test,
            ..reset
        };
        assert!(repo.reset(&missing).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn leaderboard_is_sorted_and_filterable() {
        let repo = InMemoryRouteScoreRepo::new();
        for _ in 0..3 {
            repo.record(&outcome(true), None, "v1").await.unwrap();
        }
        let mut other = outcome(false);
        other.alias = alias("gpt56-codex");
        repo.record(&other, None, "v1").await.unwrap();
        let mut test_kind = outcome(true);
        test_kind.task_kind = TaskKind::Test;
        repo.record(&test_kind, None, "v1").await.unwrap();

        let rows = repo.leaderboard(None).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].task_kind, TaskKind::Implement);
        assert_eq!(rows[0].alias.as_str(), "sonnet5-claude");
        let rows = repo.leaderboard(Some(&TaskKind::Test)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_kind, TaskKind::Test);
    }

    #[test]
    fn updated_dto_round_trips_into_the_domain_event() {
        let stats = RouteStats::from_prior(BetaPrior::UNIFORM);
        let dto = RouteScoreUpdated {
            task_kind: TaskKind::Implement,
            alias: alias("sonnet5-claude"),
            stats: stats.clone(),
            success: Some(true),
            reset: false,
            version: 7,
        };
        let event = dto.to_event();
        assert_eq!(RouteScoreUpdated::from_event(&event, 7), dto);
        assert_eq!(
            dto.stream_id(),
            RouteScore::id_for(&TaskKind::Implement, &alias("sonnet5-claude"))
        );
        assert_eq!(stats.sum_cost_usd, Decimal::ZERO);
    }
}
