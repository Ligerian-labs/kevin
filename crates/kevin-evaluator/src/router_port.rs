//! The narrow port the evaluator uses to feed routing
//! (`plan/06-memory-and-learning.md` §3.4, auto-apply `routing`).
//!
//! `kevin-evaluator` only ever needs `RecordRouteOutcome`; keeping the surface
//! to one method means the router (WS-09) can land independently and the
//! evaluator's tests never need a database.

use async_trait::async_trait;
use kevin_domain::route_score::RecordRouteOutcome;
use kevin_domain::{AttemptId, RunId, TaskId};
use std::sync::Mutex;

/// The attempt an outcome belongs to. Recording the same `attempt_id` twice
/// must leave the statistics untouched, which is what makes
/// `kevin eval rerun` idempotent (`plan/06-memory-and-learning.md` §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeAttempt {
    /// Run the attempt belonged to.
    pub run_id: RunId,
    /// Task the attempt belonged to.
    pub task_id: TaskId,
    /// The attempt itself.
    pub attempt_id: AttemptId,
}

impl OutcomeAttempt {
    /// Builds a reference from the three ids.
    #[must_use]
    pub const fn new(run_id: RunId, task_id: TaskId, attempt_id: AttemptId) -> Self {
        Self {
            run_id,
            task_id,
            attempt_id,
        }
    }
}

/// What the evaluator needs from the router.
#[async_trait]
pub trait RouterPort: Send + Sync + std::fmt::Debug {
    /// Records a terminal outcome for `(task_kind, alias)`, keyed by `attempt`
    /// when one is given.
    async fn record_outcome(
        &self,
        cmd: RecordRouteOutcome,
        attempt: Option<OutcomeAttempt>,
    ) -> Result<(), RouterPortError>;
}

/// Why an outcome could not be recorded.
#[derive(Debug, thiserror::Error)]
#[error("record route outcome: {0}")]
pub struct RouterPortError(pub String);

impl RouterPortError {
    /// Wraps any displayable error.
    pub fn new(err: impl std::fmt::Display) -> Self {
        Self(err.to_string())
    }
}

/// The real router (WS-09) implements the port directly: an outcome keyed by an
/// attempt goes through `Router::record_attempt_outcome`, which is the call that
/// makes `kevin eval rerun` idempotent.
#[async_trait]
impl RouterPort for kevin_router::Router {
    async fn record_outcome(
        &self,
        cmd: RecordRouteOutcome,
        attempt: Option<OutcomeAttempt>,
    ) -> Result<(), RouterPortError> {
        let attempt = attempt.map(|a| {
            kevin_router::AttemptRef::new(
                a.run_id.as_uuid(),
                a.task_id.as_uuid(),
                a.attempt_id.as_uuid(),
            )
        });
        kevin_router::Router::record_attempt_outcome(self, cmd, attempt)
            .await
            .map(|_| ())
            .map_err(RouterPortError::new)
    }
}

/// An in-memory [`RouterPort`] that records what it was asked to do.
/// Deduplicates by `attempt_id` exactly like the real router.
#[derive(Debug, Default)]
pub struct InMemoryRouter {
    recorded: Mutex<Vec<(RecordRouteOutcome, Option<OutcomeAttempt>)>>,
}

impl InMemoryRouter {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything recorded so far, in order.
    #[must_use]
    pub fn outcomes(&self) -> Vec<RecordRouteOutcome> {
        self.recorded
            .lock()
            .expect("router lock")
            .iter()
            .map(|(cmd, _)| cmd.clone())
            .collect()
    }

    /// Attempts recorded so far, in order.
    #[must_use]
    pub fn attempts(&self) -> Vec<Option<OutcomeAttempt>> {
        self.recorded
            .lock()
            .expect("router lock")
            .iter()
            .map(|(_, attempt)| *attempt)
            .collect()
    }

    /// How many outcomes were recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.recorded.lock().expect("router lock").len()
    }

    /// `true` when nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl RouterPort for InMemoryRouter {
    async fn record_outcome(
        &self,
        cmd: RecordRouteOutcome,
        attempt: Option<OutcomeAttempt>,
    ) -> Result<(), RouterPortError> {
        let mut recorded = self.recorded.lock().expect("router lock");
        let duplicate = attempt.is_some_and(|a| {
            recorded
                .iter()
                .any(|(_, seen)| seen.is_some_and(|seen| seen.attempt_id == a.attempt_id))
        });
        if !duplicate {
            recorded.push((cmd, attempt));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use kevin_domain::route_score::BetaPrior;
    use kevin_domain::{ModelAlias, TaskKind};

    fn cmd() -> RecordRouteOutcome {
        RecordRouteOutcome {
            task_kind: TaskKind::Implement,
            alias: ModelAlias::new("fake").unwrap(),
            success: true,
            quality: Some(0.9),
            cost_usd: None,
            wall_ms: 10,
            failure_class: None,
            recorded_at: Utc::now(),
            prior: BetaPrior::UNIFORM,
        }
    }

    #[tokio::test]
    async fn recording_the_same_attempt_twice_is_a_no_op() {
        let router = InMemoryRouter::new();
        let attempt = OutcomeAttempt::new(RunId::new(), TaskId::new(), AttemptId::new());
        router.record_outcome(cmd(), Some(attempt)).await.unwrap();
        router.record_outcome(cmd(), Some(attempt)).await.unwrap();
        assert_eq!(router.len(), 1);
        router.record_outcome(cmd(), None).await.unwrap();
        assert_eq!(router.len(), 2);
    }
}
