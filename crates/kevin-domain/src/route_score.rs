//! The [`RouteScore`] aggregate — learned statistics for one
//! `(task_kind, model_alias)` pair (`plan/02-domain-model.md` §Routing,
//! `plan/06-memory-and-learning.md` §2.4).
//!
//! ```text
//! (none) ──RecordRouteOutcome (first, with priors)──▶ scored
//! scored ──RecordRouteOutcome──▶ scored         (routing.score_updated)
//! scored ──ResetRouteScore──▶ scored             (back to priors, routing.score_updated)
//! ```
//!
//! The aggregate id is a uuid v5 of `"<task_kind>|<alias>"` so the store can
//! address the stream without a lookup table ([`RouteScore::id_for`]).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::aggregate::{Aggregate, EventMeta};
use crate::error::DomainError;
use crate::kinds::{FailureClass, ModelAlias, TaskKind};

/// Aggregate type name (`EventEnvelope::aggregate_type`).
pub const ROUTE_SCORE_AGGREGATE_TYPE: &str = "route_score";

/// uuid v5 namespace for route-score stream ids.
pub const ROUTE_SCORE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x6b, 0x3a, 0x9e, 0x2f, 0x1c, 0x8d, 0x4e, 0x7a, 0x9f, 0x02, 0x5d, 0x1e, 0x7b, 0x33, 0x44, 0x55,
]);

/// EMA weight of the newest quality sample (`quality_ema = 0.8 * old + 0.2 * new`).
pub const QUALITY_EMA_ALPHA: f32 = 0.2;

/// Beta prior `(alpha, beta)` for a fresh route score.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BetaPrior {
    /// Successes pseudo-count (≥ 1).
    pub alpha: f32,
    /// Failures pseudo-count (≥ 1).
    pub beta: f32,
}

impl BetaPrior {
    /// Uninformative prior `Beta(1, 1)`.
    pub const UNIFORM: BetaPrior = BetaPrior {
        alpha: 1.0,
        beta: 1.0,
    };

    /// Cold-start prior for a tier (`plan/06-memory-and-learning.md` §2.3).
    #[must_use]
    pub const fn for_tier(tier: crate::kinds::Tier) -> Self {
        match tier {
            crate::kinds::Tier::Frontier => BetaPrior {
                alpha: 3.0,
                beta: 1.0,
            },
            crate::kinds::Tier::Balanced => BetaPrior {
                alpha: 2.0,
                beta: 1.0,
            },
            crate::kinds::Tier::Fast => BetaPrior {
                alpha: 1.5,
                beta: 1.5,
            },
        }
    }
}

impl Default for BetaPrior {
    fn default() -> Self {
        Self::UNIFORM
    }
}

/// The learned statistics (what `routing.score_updated` carries as "stats after").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteStats {
    /// Outcomes recorded.
    pub attempts: u32,
    /// Successful outcomes.
    pub successes: u32,
    /// Sum of judge qualities (over outcomes with a quality).
    pub sum_quality: f32,
    /// Outcomes that carried a quality.
    pub quality_samples: u32,
    /// Sum of known costs over successful outcomes.
    pub sum_cost_usd: Decimal,
    /// Successful outcomes with a known cost.
    pub cost_samples: u32,
    /// Sum of wall time over successful outcomes (ms).
    pub sum_wall_ms: u64,
    /// Beta posterior alpha.
    pub alpha: f32,
    /// Beta posterior beta.
    pub beta: f32,
    /// Exponential moving average of quality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_ema: Option<f32>,
    /// When the route was last used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
}

impl RouteStats {
    /// Fresh stats from a prior.
    #[must_use]
    pub const fn from_prior(prior: BetaPrior) -> Self {
        Self {
            attempts: 0,
            successes: 0,
            sum_quality: 0.0,
            quality_samples: 0,
            sum_cost_usd: Decimal::ZERO,
            cost_samples: 0,
            sum_wall_ms: 0,
            alpha: prior.alpha,
            beta: prior.beta,
            quality_ema: None,
            last_used: None,
        }
    }

    /// `successes / attempts` (0 when unused).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn win_rate(&self) -> f32 {
        if self.attempts == 0 {
            0.0
        } else {
            self.successes as f32 / self.attempts as f32
        }
    }

    /// `alpha / (alpha + beta)`.
    #[must_use]
    pub fn p_success(&self) -> f32 {
        self.alpha / (self.alpha + self.beta)
    }

    /// Mean quality over sampled outcomes.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn mean_quality(&self) -> Option<f32> {
        (self.quality_samples > 0).then(|| self.sum_quality / self.quality_samples as f32)
    }

    /// Mean cost over successful outcomes with a known cost.
    #[must_use]
    pub fn mean_cost_usd(&self) -> Option<Decimal> {
        (self.cost_samples > 0)
            .then(|| {
                self.sum_cost_usd
                    .checked_div(Decimal::from(self.cost_samples))
            })
            .flatten()
    }

    /// Mean wall time over successful outcomes.
    #[must_use]
    pub fn mean_wall_ms(&self) -> Option<u64> {
        (self.successes > 0).then(|| self.sum_wall_ms / u64::from(self.successes))
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Records a terminal task outcome against a route (`routing.score_updated`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordRouteOutcome {
    /// Task kind.
    pub task_kind: TaskKind,
    /// Model alias.
    pub alias: ModelAlias,
    /// The attempt succeeded.
    pub success: bool,
    /// Judge overall (0..=1) when evaluated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<f32>,
    /// Attempt cost when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<Decimal>,
    /// Attempt wall time.
    pub wall_ms: u64,
    /// Failure class when `!success`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<FailureClass>,
    /// When the outcome was recorded (becomes `last_used`).
    pub recorded_at: DateTime<Utc>,
    /// Prior used when this is the first outcome for the pair.
    #[serde(default)]
    pub prior: BetaPrior,
}

/// Resets a route score to a prior (`kevin routes reset`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetRouteScore {
    /// Task kind.
    pub task_kind: TaskKind,
    /// Model alias.
    pub alias: ModelAlias,
    /// Prior to reset to.
    #[serde(default)]
    pub prior: BetaPrior,
}

/// Every command the [`RouteScore`] aggregate handles.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteScoreCommand {
    /// [`RecordRouteOutcome`].
    RecordOutcome(RecordRouteOutcome),
    /// [`ResetRouteScore`].
    Reset(ResetRouteScore),
}

impl RouteScoreCommand {
    /// `snake_case` command name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            RouteScoreCommand::RecordOutcome(_) => "record_route_outcome",
            RouteScoreCommand::Reset(_) => "reset_route_score",
        }
    }
}

impl From<RecordRouteOutcome> for RouteScoreCommand {
    fn from(cmd: RecordRouteOutcome) -> Self {
        RouteScoreCommand::RecordOutcome(cmd)
    }
}

impl From<ResetRouteScore> for RouteScoreCommand {
    fn from(cmd: ResetRouteScore) -> Self {
        RouteScoreCommand::Reset(cmd)
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events of the `route_score` stream (internally tagged on `type`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RouteScoreEvent {
    /// `routing.score_updated`
    #[serde(rename = "routing.score_updated")]
    ScoreUpdated {
        /// Task kind.
        task_kind: TaskKind,
        /// Model alias.
        alias: ModelAlias,
        /// Stats after the update.
        stats: RouteStats,
        /// The outcome was a success (`None` for resets).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        /// `true` when this update is a reset to priors.
        #[serde(default)]
        reset: bool,
    },
}

impl RouteScoreEvent {
    /// Every event type of the `route_score` stream.
    pub const TYPES: [&'static str; 1] = ["routing.score_updated"];
}

impl EventMeta for RouteScoreEvent {
    fn event_type(&self) -> &'static str {
        match self {
            RouteScoreEvent::ScoreUpdated { .. } => "routing.score_updated",
        }
    }

    fn schema_version(&self) -> u16 {
        1
    }

    fn aggregate_type(&self) -> &'static str {
        ROUTE_SCORE_AGGREGATE_TYPE
    }
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// Learned statistics for one `(task_kind, alias)` pair.
#[derive(Debug, Clone, Default)]
pub struct RouteScore {
    version: u64,
    task_kind: Option<TaskKind>,
    alias: Option<ModelAlias>,
    stats: Option<RouteStats>,
}

impl RouteScore {
    /// Deterministic stream id for a pair.
    #[must_use]
    pub fn id_for(task_kind: &TaskKind, alias: &ModelAlias) -> Uuid {
        Uuid::new_v5(
            &ROUTE_SCORE_NAMESPACE,
            format!("{task_kind}|{alias}").as_bytes(),
        )
    }

    /// Task kind (after the first event).
    #[must_use]
    pub const fn task_kind(&self) -> Option<&TaskKind> {
        self.task_kind.as_ref()
    }

    /// Alias (after the first event).
    #[must_use]
    pub const fn alias(&self) -> Option<&ModelAlias> {
        self.alias.as_ref()
    }

    /// Current stats (after the first event).
    #[must_use]
    pub const fn stats(&self) -> Option<&RouteStats> {
        self.stats.as_ref()
    }

    fn check_pair(&self, task_kind: &TaskKind, alias: &ModelAlias) -> Result<(), DomainError> {
        if let (Some(k), Some(a)) = (&self.task_kind, &self.alias)
            && (k != task_kind || a != alias)
        {
            return Err(DomainError::invalid_value(
                "task_kind/alias",
                format!("stream belongs to {k}|{a}, not {task_kind}|{alias}"),
            ));
        }
        Ok(())
    }

    fn next_stats(&self, cmd: &RecordRouteOutcome) -> Result<RouteStats, DomainError> {
        if let Some(q) = cmd.quality
            && !(0.0..=1.0).contains(&q)
        {
            return Err(DomainError::invalid_value(
                "quality",
                "must be within 0..=1",
            ));
        }
        if let Some(c) = cmd.cost_usd
            && c.is_sign_negative()
        {
            return Err(DomainError::invalid_value(
                "cost_usd",
                "must not be negative",
            ));
        }
        if cmd.prior.alpha < 1.0 || cmd.prior.beta < 1.0 {
            return Err(DomainError::invalid_value(
                "prior",
                "alpha and beta must be ≥ 1",
            ));
        }
        let mut stats = self
            .stats
            .clone()
            .unwrap_or_else(|| RouteStats::from_prior(cmd.prior));
        stats.attempts = stats.attempts.saturating_add(1);
        if cmd.success {
            stats.successes = stats.successes.saturating_add(1);
            stats.alpha += 1.0;
            stats.sum_wall_ms = stats.sum_wall_ms.saturating_add(cmd.wall_ms);
            if let Some(cost) = cmd.cost_usd {
                stats.sum_cost_usd = stats.sum_cost_usd.saturating_add(cost);
                stats.cost_samples = stats.cost_samples.saturating_add(1);
            }
        } else if cmd.failure_class.is_some_and(FailureClass::blames_model) {
            stats.beta += 1.0;
        }
        if let Some(q) = cmd.quality {
            stats.sum_quality += q;
            stats.quality_samples = stats.quality_samples.saturating_add(1);
            stats.quality_ema = Some(match stats.quality_ema {
                Some(old) => (1.0 - QUALITY_EMA_ALPHA) * old + QUALITY_EMA_ALPHA * q,
                None => q,
            });
        }
        stats.last_used = Some(cmd.recorded_at);
        Ok(stats)
    }
}

impl Aggregate for RouteScore {
    type Command = RouteScoreCommand;
    type Event = RouteScoreEvent;

    const TYPE: &'static str = ROUTE_SCORE_AGGREGATE_TYPE;

    fn id(&self) -> Uuid {
        match (&self.task_kind, &self.alias) {
            (Some(k), Some(a)) => Self::id_for(k, a),
            _ => Uuid::nil(),
        }
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn handle(&self, cmd: &RouteScoreCommand) -> Result<Vec<RouteScoreEvent>, DomainError> {
        match cmd {
            RouteScoreCommand::RecordOutcome(c) => {
                self.check_pair(&c.task_kind, &c.alias)?;
                let stats = self.next_stats(c)?;
                Ok(vec![RouteScoreEvent::ScoreUpdated {
                    task_kind: c.task_kind.clone(),
                    alias: c.alias.clone(),
                    stats,
                    success: Some(c.success),
                    reset: false,
                }])
            }
            RouteScoreCommand::Reset(c) => {
                self.check_pair(&c.task_kind, &c.alias)?;
                if c.prior.alpha < 1.0 || c.prior.beta < 1.0 {
                    return Err(DomainError::invalid_value(
                        "prior",
                        "alpha and beta must be ≥ 1",
                    ));
                }
                let mut stats = RouteStats::from_prior(c.prior);
                stats.last_used = self.stats.as_ref().and_then(|s| s.last_used);
                Ok(vec![RouteScoreEvent::ScoreUpdated {
                    task_kind: c.task_kind.clone(),
                    alias: c.alias.clone(),
                    stats,
                    success: None,
                    reset: true,
                }])
            }
        }
    }

    fn apply(&mut self, event: &RouteScoreEvent) {
        self.version += 1;
        match event {
            RouteScoreEvent::ScoreUpdated {
                task_kind,
                alias,
                stats,
                ..
            } => {
                self.task_kind = Some(task_kind.clone());
                self.alias = Some(alias.clone());
                self.stats = Some(stats.clone());
            }
        }
    }
}
