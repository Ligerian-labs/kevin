//! The Kohral metrics of `plan/10-observability-ops.md` §Metrics.
//!
//! Three series, and they live here rather than inline so that the outcome
//! label stays a bounded enum: plan/10 documents
//! `kevin_kohral_turns_total{outcome}` with exactly the values below, and a
//! label a caller can invent is how a Prometheus cardinality incident starts.
//!
//! `kevin_kohral_turns_active` is maintained two ways on purpose: the
//! acceptance and terminal paths move it by one so it is right *between*
//! scrapes, and `/health/detailed` reconciles it against `count(*)` on the
//! ledger, so a crash mid-turn cannot leave the gauge permanently skewed.

use kevin_telemetry::metrics as names;

use crate::ledger::TurnStatus;

/// `outcome` label values (`plan/10` §Metrics).
pub mod outcome {
    /// The turn produced an answer.
    pub const COMPLETED: &str = "completed";
    /// The turn failed.
    pub const FAILED: &str = "failed";
    /// The turn was stopped (Kohral shows `interrupted`).
    pub const INTERRUPTED: &str = "interrupted";
    /// A restart terminalised the turn.
    pub const RUNTIME_RESTARTED: &str = "runtime_restarted";
    /// The same `Idempotency-Key` and request came back.
    pub const IDEMPOTENT_REPLAY: &str = "idempotent_replay";
    /// The same key came back with a different request.
    pub const CONFLICT: &str = "conflict";
}

/// A turn was accepted: one more turn in flight.
pub fn turn_accepted() {
    metrics::gauge!(names::KOHRAL_TURNS_ACTIVE).increment(1.0);
}

/// Kohral re-sent a turn Kevin had already accepted.
pub fn turn_replayed() {
    metrics::counter!(names::KOHRAL_TURNS_TOTAL, "outcome" => outcome::IDEMPOTENT_REPLAY)
        .increment(1);
}

/// Kohral re-used a key with a different request.
pub fn turn_conflicted() {
    metrics::counter!(names::KOHRAL_TURNS_TOTAL, "outcome" => outcome::CONFLICT).increment(1);
}

/// A turn reached a terminal status.
pub fn turn_terminal(status: TurnStatus, error_code: Option<&str>) {
    let outcome = match status {
        TurnStatus::Completed => outcome::COMPLETED,
        TurnStatus::Cancelled => outcome::INTERRUPTED,
        TurnStatus::Failed if error_code == Some(crate::ledger::RUNTIME_RESTARTED) => {
            outcome::RUNTIME_RESTARTED
        }
        TurnStatus::Failed => outcome::FAILED,
        // Not terminal: nothing to count.
        TurnStatus::Queued | TurnStatus::Running | TurnStatus::Stopping => return,
    };
    metrics::counter!(names::KOHRAL_TURNS_TOTAL, "outcome" => outcome).increment(1);
    metrics::gauge!(names::KOHRAL_TURNS_ACTIVE).decrement(1.0);
}

/// The boot sweep terminalised `turns` turns.
pub fn turns_restarted(turns: usize) {
    metrics::counter!(names::KOHRAL_TURNS_TOTAL, "outcome" => outcome::RUNTIME_RESTARTED)
        .increment(turns as u64);
}

/// Reconciles the in-flight gauge with the ledger.
pub fn active_turns(count: i64) {
    #[allow(clippy::cast_precision_loss)]
    metrics::gauge!(names::KOHRAL_TURNS_ACTIVE).set(count as f64);
}

/// `kevin_kohral_draining` (0/1).
pub fn draining(draining: bool) {
    metrics::gauge!(names::KOHRAL_DRAINING).set(f64::from(u8::from(draining)));
}

#[cfg(test)]
mod tests {
    use super::outcome;
    use crate::ledger::TurnStatus;

    /// The outcomes plan/10 §Metrics documents, and no others.
    #[test]
    fn the_outcome_label_is_a_bounded_enum() {
        let documented = [
            outcome::COMPLETED,
            outcome::FAILED,
            outcome::INTERRUPTED,
            outcome::RUNTIME_RESTARTED,
            outcome::IDEMPOTENT_REPLAY,
            outcome::CONFLICT,
        ];
        assert_eq!(documented.len(), 6);
        for label in documented {
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{label}"
            );
        }
    }

    /// Every terminal status maps onto one of them; no non-terminal one does.
    #[test]
    fn every_terminal_status_has_an_outcome() {
        for status in TurnStatus::ALL {
            let mapped = matches!(
                status,
                TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Cancelled
            );
            assert_eq!(mapped, status.is_terminal(), "{}", status.as_str());
        }
    }
}
