//! Hybrid ranking (`plan/06-memory-and-learning.md` §1.4).
//!
//! ```text
//! similarity = 1 - cosine_distance                  -- 0..1 (0 when embedding NULL)
//! lexical    = ts_rank_cd(...) normalised to 0..1 over the candidate set
//! decay      = 0.5 ^ (age_days / memory.decay_half_life_days)
//! score      = 0.60*similarity + 0.25*lexical + 0.15*(importance*(0.5 + 0.5*decay))
//! ```
//!
//! Decay affects ranking only; nothing is ever deleted by age.

/// Weight of the vector similarity.
pub const W_SIMILARITY: f32 = 0.60;
/// Weight of the lexical (`ts_rank_cd`) score.
pub const W_LEXICAL: f32 = 0.25;
/// Weight of the decayed importance.
pub const W_IMPORTANCE: f32 = 0.15;

/// `0.5 ^ (age_days / half_life_days)`, clamped to `0..=1`.
#[must_use]
pub fn decay(age_days: f32, half_life_days: f32) -> f32 {
    if half_life_days <= 0.0 {
        return 0.0;
    }
    0.5f32
        .powf(age_days.max(0.0) / half_life_days)
        .clamp(0.0, 1.0)
}

/// The hybrid score of one candidate.
#[must_use]
pub fn hybrid_score(similarity: f32, lexical: f32, importance: f32, decay: f32) -> f32 {
    W_SIMILARITY * similarity.clamp(0.0, 1.0)
        + W_LEXICAL * lexical.clamp(0.0, 1.0)
        + W_IMPORTANCE * (importance.clamp(0.0, 1.0) * (0.5 + 0.5 * decay.clamp(0.0, 1.0)))
}

/// Normalises raw `ts_rank_cd` values to `0..1` over the candidate set (the
/// best candidate becomes `1`; an all-zero set stays all-zero).
#[must_use]
pub fn normalise_lexical(raw: &[f32]) -> Vec<f32> {
    let max = raw.iter().copied().fold(0.0f32, f32::max);
    if max <= 0.0 {
        return vec![0.0; raw.len()];
    }
    raw.iter().map(|v| (v / max).clamp(0.0, 1.0)).collect()
}

/// Age of an item in (fractional) days, never negative.
///
/// Precision loss is irrelevant here: the value feeds an exponential decay
/// measured in days, where `f32` resolution is far finer than the half-life.
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn age_days(
    created_at: chrono::DateTime<chrono::Utc>,
    now: chrono::DateTime<chrono::Utc>,
) -> f32 {
    let millis = (now - created_at).num_milliseconds().max(0) as f64;
    (millis / 86_400_000.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_halves_every_half_life() {
        assert!((decay(0.0, 90.0) - 1.0).abs() < 1e-6);
        assert!((decay(90.0, 90.0) - 0.5).abs() < 1e-6);
        assert!((decay(180.0, 90.0) - 0.25).abs() < 1e-6);
        assert!((decay(-5.0, 90.0) - 1.0).abs() < f32::EPSILON);
        assert!(decay(10.0, 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn weights_sum_to_one_and_a_perfect_hit_scores_one() {
        assert!((W_SIMILARITY + W_LEXICAL + W_IMPORTANCE - 1.0).abs() < 1e-6);
        assert!((hybrid_score(1.0, 1.0, 1.0, 1.0) - 1.0).abs() < 1e-6);
        assert!(hybrid_score(0.0, 0.0, 0.0, 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn age_only_lowers_the_importance_term() {
        let fresh = hybrid_score(0.5, 0.0, 1.0, decay(0.0, 90.0));
        let old = hybrid_score(0.5, 0.0, 1.0, decay(360.0, 90.0));
        assert!(fresh > old);
        // The vector/lexical part is untouched: the gap is at most the
        // importance weight halved.
        assert!(fresh - old <= W_IMPORTANCE * 0.5 + 1e-6);
    }

    #[test]
    fn lexical_normalisation_is_relative_to_the_best_candidate() {
        assert!(
            normalise_lexical(&[0.0, 0.0])
                .iter()
                .all(|v| v.abs() < f32::EPSILON)
        );
        let normalised = normalise_lexical(&[0.2, 0.1, 0.0]);
        for (got, want) in normalised.iter().zip([1.0f32, 0.5, 0.0]) {
            assert!((got - want).abs() < 1e-6, "{normalised:?}");
        }
    }
}
