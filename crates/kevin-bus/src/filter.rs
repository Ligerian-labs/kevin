//! Subscription filters.

use uuid::Uuid;

use crate::Event;

/// What a subscriber wants to see. `None` = no constraint on that axis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionFilter {
    /// Only events whose `correlation_id` is this run.
    pub run_id: Option<Uuid>,
    /// Only these `event_type`s (`"run.started"`, …).
    pub event_types: Option<Vec<String>>,
    /// Only these `aggregate_type`s (`"run"`, `"task"`, …).
    pub aggregate_types: Option<Vec<String>>,
    /// Bounded subscriber name used as the `subscriber` metric label and in
    /// lag logs (`"sse"`, `"projection:task_board"`…). Never an id.
    pub subscriber: Option<String>,
}

impl SubscriptionFilter {
    /// Everything.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Everything for one run.
    #[must_use]
    pub fn for_run(run_id: Uuid) -> Self {
        Self {
            run_id: Some(run_id),
            ..Self::default()
        }
    }

    /// Restricts to the given event types.
    #[must_use]
    pub fn with_event_types<I, S>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.event_types = Some(types.into_iter().map(Into::into).collect());
        self
    }

    /// Restricts to the given aggregate types.
    #[must_use]
    pub fn with_aggregate_types<I, S>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.aggregate_types = Some(types.into_iter().map(Into::into).collect());
        self
    }

    /// Names the subscriber (metrics label).
    #[must_use]
    pub fn named(mut self, subscriber: impl Into<String>) -> Self {
        self.subscriber = Some(subscriber.into());
        self
    }

    /// Whether `event` passes the filter.
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        let run_ok = self
            .run_id
            .is_none_or(|run_id| event.correlation_id == run_id);
        let type_ok = self
            .event_types
            .as_ref()
            .is_none_or(|types| types.iter().any(|t| t == event.event_type));
        let aggregate_ok = self
            .aggregate_types
            .as_ref()
            .is_none_or(|types| types.iter().any(|t| t == event.aggregate_type));
        run_ok && type_ok && aggregate_ok
    }

    /// The label value for `kevin_bus_lagged_total{subscriber}`.
    #[must_use]
    pub fn subscriber_label(&self) -> String {
        self.subscriber
            .clone()
            .unwrap_or_else(|| "unnamed".to_owned())
    }
}
