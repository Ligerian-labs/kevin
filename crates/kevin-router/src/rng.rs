//! Randomness for route selection (`plan/11-testing.md` §Determinism rules).
//!
//! The router never touches a global RNG: it asks an injected [`RngSource`] for
//! one `StdRng` per selection. [`SeededRngSource`] makes a whole sequence of
//! selections reproducible (same base seed → same sequence), and
//! `SelectRouteQuery::rng_seed` pins one single selection.
//!
//! Beta sampling is implemented here (Marsaglia–Tsang gamma variates) rather
//! than pulled from `rand_distr`, so the sampling is deterministic under our
//! own seeding and needs no extra dependency.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};

/// Source of per-selection RNGs.
pub trait RngSource: Send + Sync + fmt::Debug {
    /// A fresh RNG for one selection.
    fn rng(&self) -> StdRng;
}

/// OS-seeded source (production default).
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRngSource;

impl RngSource for OsRngSource {
    fn rng(&self) -> StdRng {
        rand::make_rng()
    }
}

/// Deterministic source: the n-th call is seeded with `base ^ n`, so a fixed
/// base seed replays the exact same sequence of selections.
#[derive(Debug)]
pub struct SeededRngSource {
    base: u64,
    calls: AtomicU64,
}

impl SeededRngSource {
    /// A source starting from `base`.
    #[must_use]
    pub const fn new(base: u64) -> Self {
        Self {
            base,
            calls: AtomicU64::new(0),
        }
    }

    /// How many RNGs were handed out so far.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }
}

impl RngSource for SeededRngSource {
    fn rng(&self) -> StdRng {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        StdRng::seed_from_u64(self.base ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }
}

/// A source that always returns the same seed (one reproducible selection).
#[derive(Debug, Clone, Copy)]
pub struct FixedSeedRngSource(pub u64);

impl RngSource for FixedSeedRngSource {
    fn rng(&self) -> StdRng {
        StdRng::seed_from_u64(self.0)
    }
}

/// Samples `Beta(alpha, beta)` as `X / (X + Y)` with `X ~ Gamma(alpha, 1)` and
/// `Y ~ Gamma(beta, 1)`. Both parameters must be finite and > 0; degenerate
/// inputs fall back to the mean `alpha / (alpha + beta)`.
pub fn sample_beta<R: Rng + ?Sized>(rng: &mut R, alpha: f32, beta: f32) -> f32 {
    if !(alpha.is_finite() && beta.is_finite()) || alpha <= 0.0 || beta <= 0.0 {
        let sum = alpha + beta;
        return if sum > 0.0 { alpha / sum } else { 0.5 };
    }
    let x = sample_gamma(rng, f64::from(alpha));
    let y = sample_gamma(rng, f64::from(beta));
    let sum = x + y;
    if sum <= 0.0 {
        return alpha / (alpha + beta);
    }
    #[allow(clippy::cast_possible_truncation)]
    let sampled = (x / sum) as f32;
    sampled.clamp(0.0, 1.0)
}

/// Samples `Gamma(shape, 1)` (Marsaglia–Tsang, with Johnk's boost for
/// `shape < 1`).
fn sample_gamma<R: Rng + ?Sized>(rng: &mut R, shape: f64) -> f64 {
    if shape < 1.0 {
        let uniform: f64 = positive_unit(rng);
        return sample_gamma(rng, shape + 1.0) * uniform.powf(1.0 / shape);
    }
    let shifted = shape - 1.0 / 3.0;
    let scale = 1.0 / (9.0 * shifted).sqrt();
    loop {
        let normal = standard_normal(rng);
        let root = 1.0 + scale * normal;
        if root <= 0.0 {
            continue;
        }
        let cubed = root * root * root;
        let uniform: f64 = positive_unit(rng);
        let normal4 = normal * normal * normal * normal;
        if uniform < 1.0 - 0.033_1 * normal4 {
            return shifted * cubed;
        }
        if uniform.ln() < 0.5 * normal * normal + shifted * (1.0 - cubed + cubed.ln()) {
            return shifted * cubed;
        }
    }
}

/// Standard normal variate (Box–Muller).
fn standard_normal<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let radial: f64 = positive_unit(rng);
    let angular: f64 = rng.random();
    (-2.0 * radial.ln()).sqrt() * (std::f64::consts::TAU * angular).cos()
}

/// Uniform in `(0, 1]` — never zero, so `ln` stays finite.
fn positive_unit<R: Rng + ?Sized>(rng: &mut R) -> f64 {
    let uniform: f64 = rng.random();
    if uniform <= 0.0 {
        f64::MIN_POSITIVE
    } else {
        uniform
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_source_replays_the_same_sequence() {
        let a = SeededRngSource::new(7);
        let b = SeededRngSource::new(7);
        for _ in 0..5 {
            let mut ra = a.rng();
            let mut rb = b.rng();
            let (left, right) = (
                sample_beta(&mut ra, 2.0, 3.0),
                sample_beta(&mut rb, 2.0, 3.0),
            );
            assert!((left - right).abs() < f32::EPSILON, "{left} != {right}");
        }
        assert_eq!(a.calls(), 5);
    }

    #[test]
    fn consecutive_seeded_draws_differ() {
        let source = SeededRngSource::new(1);
        let first = sample_beta(&mut source.rng(), 2.0, 2.0);
        let second = sample_beta(&mut source.rng(), 2.0, 2.0);
        assert!((first - second).abs() > f32::EPSILON);
    }

    #[test]
    fn beta_samples_stay_in_range_and_track_the_mean() {
        let mut rng = StdRng::seed_from_u64(42);
        let (alpha, beta) = (8.0_f32, 2.0_f32);
        let n = 2_000;
        let mut sum = 0.0_f64;
        for _ in 0..n {
            let s = sample_beta(&mut rng, alpha, beta);
            assert!((0.0..=1.0).contains(&s), "sample out of range: {s}");
            sum += f64::from(s);
        }
        #[allow(clippy::cast_lossless)]
        let mean = sum / f64::from(n);
        let expected = f64::from(alpha / (alpha + beta));
        assert!(
            (mean - expected).abs() < 0.02,
            "mean {mean} far from {expected}"
        );
    }

    #[test]
    fn degenerate_parameters_fall_back_to_the_mean() {
        let mut rng = StdRng::seed_from_u64(1);
        assert!((sample_beta(&mut rng, 0.0, 0.0) - 0.5).abs() < f32::EPSILON);
        assert!((sample_beta(&mut rng, 3.0, 1.0) - 0.75).abs() < 1.0);
        assert!(sample_beta(&mut rng, -1.0, 3.0).is_finite());
    }

    #[test]
    fn small_shapes_are_supported() {
        let mut rng = StdRng::seed_from_u64(3);
        for _ in 0..200 {
            let s = sample_beta(&mut rng, 0.5, 0.5);
            assert!((0.0..=1.0).contains(&s));
        }
    }
}
