//! Property tests of the route-score update rules
//! (`plan/11-testing.md` §kevin-router, `plan/06-memory-and-learning.md` §2.4).

use chrono::{TimeDelta, Utc};
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome};
use kevin_domain::{Decimal, FailureClass, ModelAlias, RouteStats, TaskKind, Tier};
use kevin_router::next_stats;
use proptest::prelude::*;
use rust_decimal::prelude::FromPrimitive;

#[derive(Debug, Clone)]
struct Step {
    success: bool,
    class: FailureClass,
    quality: Option<f32>,
    cost_cents: Option<u32>,
    wall_ms: u32,
}

fn step() -> impl Strategy<Value = Step> {
    (
        any::<bool>(),
        prop_oneof![
            Just(FailureClass::Transient),
            Just(FailureClass::Permanent),
            Just(FailureClass::Budget),
            Just(FailureClass::Cancelled),
            Just(FailureClass::RuntimeRestarted),
        ],
        proptest::option::of(0.0f32..=1.0f32),
        proptest::option::of(0u32..100_000),
        0u32..3_600_000,
    )
        .prop_map(|(success, class, quality, cost_cents, wall_ms)| Step {
            success,
            class,
            quality,
            cost_cents,
            wall_ms,
        })
}

fn apply(stats: Option<&RouteStats>, index: usize, step: &Step) -> RouteStats {
    let outcome = RecordRouteOutcome {
        task_kind: TaskKind::Implement,
        alias: ModelAlias::new("sonnet5-claude").expect("alias"),
        success: step.success,
        quality: step.quality,
        cost_usd: step
            .cost_cents
            .and_then(|c| Decimal::from_f64(f64::from(c) / 100.0)),
        wall_ms: u64::from(step.wall_ms),
        failure_class: (!step.success).then_some(step.class),
        recorded_at: Utc::now() + TimeDelta::seconds(i64::try_from(index).unwrap_or(0)),
        prior: BetaPrior::for_tier(Tier::Balanced),
    };
    next_stats(stats, &outcome).expect("valid outcome")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// `alpha`/`beta` never decrease, grow by at most one per outcome, and only
    /// `Permanent`/`Budget` failures move `beta`.
    #[test]
    fn alpha_and_beta_are_monotone(steps in proptest::collection::vec(step(), 1..40)) {
        let prior = BetaPrior::for_tier(Tier::Balanced);
        let mut stats: Option<RouteStats> = None;
        for (index, step) in steps.iter().enumerate() {
            let before = stats.clone();
            let after = apply(before.as_ref(), index, step);
            let (old_alpha, old_beta) = before
                .as_ref()
                .map_or((prior.alpha, prior.beta), |s| (s.alpha, s.beta));
            prop_assert!(after.alpha >= old_alpha, "alpha decreased");
            prop_assert!(after.beta >= old_beta, "beta decreased");
            prop_assert!(after.alpha - old_alpha <= 1.0);
            prop_assert!(after.beta - old_beta <= 1.0);
            prop_assert!(after.alpha >= 1.0 && after.beta >= 1.0);
            if step.success {
                prop_assert!((after.alpha - old_alpha - 1.0).abs() < f32::EPSILON);
                prop_assert!((after.beta - old_beta).abs() < f32::EPSILON);
            } else if step.class.blames_model() {
                prop_assert!((after.beta - old_beta - 1.0).abs() < f32::EPSILON);
            } else {
                prop_assert!((after.beta - old_beta).abs() < f32::EPSILON);
                prop_assert!((after.alpha - old_alpha).abs() < f32::EPSILON);
            }
            stats = Some(after);
        }
        let stats = stats.expect("at least one outcome");
        prop_assert_eq!(stats.attempts as usize, steps.len());
        prop_assert!(stats.successes <= stats.attempts);
        let successes = steps.iter().filter(|s| s.success).count();
        prop_assert_eq!(stats.successes as usize, successes);
    }

    /// The quality EMA stays inside the range of the samples that fed it, and
    /// the derived means stay finite and non-negative.
    #[test]
    fn quality_ema_and_means_stay_bounded(steps in proptest::collection::vec(step(), 1..40)) {
        let mut stats: Option<RouteStats> = None;
        for (index, step) in steps.iter().enumerate() {
            stats = Some(apply(stats.as_ref(), index, step));
        }
        let stats = stats.expect("at least one outcome");
        let qualities: Vec<f32> = steps.iter().filter_map(|s| s.quality).collect();
        match stats.quality_ema {
            None => prop_assert!(qualities.is_empty()),
            Some(ema) => {
                prop_assert!(!qualities.is_empty());
                prop_assert!((0.0..=1.0).contains(&ema), "ema out of range: {}", ema);
                let lo = qualities.iter().copied().fold(f32::INFINITY, f32::min);
                let hi = qualities.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                prop_assert!(ema >= lo - 1e-5 && ema <= hi + 1e-5);
            }
        }
        prop_assert_eq!(stats.quality_samples as usize, qualities.len());
        if let Some(mean) = stats.mean_quality() {
            prop_assert!((0.0..=1.0).contains(&mean));
        }
        if let Some(cost) = stats.mean_cost_usd() {
            prop_assert!(cost >= Decimal::ZERO);
        }
        if let Some(wall) = stats.mean_wall_ms() {
            prop_assert!(wall <= 3_600_000);
        }
        prop_assert!(stats.p_success() > 0.0 && stats.p_success() < 1.0);
        prop_assert!((0.0..=1.0).contains(&stats.win_rate()));
    }
}
