//! Stable event names (`plan/10-observability-ops.md` §Logging), prefix `kevin.`.
//!
//! Emit them as the `event` field of a tracing record:
//!
//! ```
//! use kevin_telemetry::{events, fields};
//! tracing::info!({ fields::EVENT } = events::run::STARTED, "run started");
//! ```

/// Name prefix shared by every event.
pub const PREFIX: &str = "kevin.";

macro_rules! event_names {
    ($(#[$m:meta])* $modname:ident : $( $konst:ident = $name:literal ),+ $(,)?) => {
        $(#[$m])*
        pub mod $modname {
            $( #[doc = concat!("`", $name, "`")] pub const $konst: &str = $name; )+
            /// Every event name in this group.
            pub const ALL: &[&str] = &[$($konst),+];
        }
    };
}

event_names!(
    /// Startup lifecycle.
    startup: CONFIG_LOADED = "kevin.startup.config_loaded", READY = "kevin.startup.ready",
);
event_names!(
    /// Shutdown lifecycle.
    shutdown: BEGIN = "kevin.shutdown.begin", DRAINED = "kevin.shutdown.drained", FORCED = "kevin.shutdown.forced",
);
event_names!(
    /// Run lifecycle.
    run:
    STARTED = "kevin.run.started",
    UNDERSTANDING_COMPLETED = "kevin.run.understanding_completed",
    QUESTIONS_ASKED = "kevin.run.questions_asked",
    PLAN_PROPOSED = "kevin.run.plan_proposed",
    PLAN_APPROVED = "kevin.run.plan_approved",
    EXECUTING = "kevin.run.executing",
    INTEGRATED = "kevin.run.integrated",
    EVALUATED = "kevin.run.evaluated",
    COMPLETED = "kevin.run.completed",
    FAILED = "kevin.run.failed",
    CANCELLED = "kevin.run.cancelled",
    BUDGET_EXHAUSTED = "kevin.run.budget_exhausted",
);
event_names!(
    /// Task lifecycle.
    task:
    CREATED = "kevin.task.created",
    ROUTED = "kevin.task.routed",
    ATTEMPT_STARTED = "kevin.task.attempt_started",
    PROGRESSED = "kevin.task.progressed",
    ATTEMPT_SUCCEEDED = "kevin.task.attempt_succeeded",
    ATTEMPT_FAILED = "kevin.task.attempt_failed",
    RETRIED = "kevin.task.retried",
    SKIPPED = "kevin.task.skipped",
    CANCELLED = "kevin.task.cancelled",
);
event_names!(
    /// Question lifecycle.
    question: ASKED = "kevin.question.asked", ANSWERED = "kevin.question.answered", EXPIRED = "kevin.question.expired",
);
event_names!(
    /// Worker subprocesses. `STDOUT_LINE`/`STDERR_LINE` are `debug` and sampled.
    worker:
    SPAWNED = "kevin.worker.spawned",
    STDOUT_LINE = "kevin.worker.stdout_line",
    STDERR_LINE = "kevin.worker.stderr_line",
    EXITED = "kevin.worker.exited",
    KILLED = "kevin.worker.killed",
    TIMEOUT = "kevin.worker.timeout",
    POLICY_VIOLATION = "kevin.worker.policy_violation",
);
event_names!(
    /// Workspaces.
    workspace: CREATED = "kevin.workspace.created", REMOVED = "kevin.workspace.removed", ESCAPE_DETECTED = "kevin.workspace.escape_detected",
);
event_names!(
    /// Router.
    router: SELECTED = "kevin.router.selected", SCORE_UPDATED = "kevin.router.score_updated",
);
event_names!(
    /// Memory.
    memory: STORED = "kevin.memory.stored", RETRIEVED = "kevin.memory.retrieved", FORGOTTEN = "kevin.memory.forgotten", REINDEXED = "kevin.memory.reindexed",
);
event_names!(
    /// Evaluation.
    eval:
    RECORDED = "kevin.eval.recorded",
    PROPOSAL_RAISED = "kevin.eval.proposal_raised",
    PROPOSAL_ACCEPTED = "kevin.eval.proposal_accepted",
    PROPOSAL_REJECTED = "kevin.eval.proposal_rejected",
);
event_names!(
    /// Event store, outbox, projections.
    store:
    APPENDED = "kevin.store.appended",
    VERSION_CONFLICT = "kevin.store.version_conflict",
    OUTBOX_RELAYED = "kevin.store.outbox_relayed",
    PROJECTION_CHECKPOINT = "kevin.store.projection_checkpoint",
    PROJECTION_REBUILT = "kevin.store.projection_rebuilt",
);
event_names!(
    /// Event bus.
    bus: LAGGED = "kevin.bus.lagged",
);
event_names!(
    /// HTTP API.
    api: REQUEST = "kevin.api.request", AUTH_FAILED = "kevin.api.auth_failed",
);
event_names!(
    /// Kohral adapter.
    kohral:
    TURN_ACCEPTED = "kevin.kohral.turn_accepted",
    TURN_TERMINAL = "kevin.kohral.turn_terminal",
    DRAIN_CHANGED = "kevin.kohral.drain_changed",
    RUNTIME_RESTARTED = "kevin.kohral.runtime_restarted",
);
event_names!(
    /// Sandbox.
    sandbox: DISABLED = "kevin.sandbox.disabled",
);
event_names!(
    /// Budgets.
    budget: WARNING = "kevin.budget.warning",
);

/// Every event name, for tests and documentation generators.
#[must_use]
pub fn all() -> Vec<&'static str> {
    [
        startup::ALL,
        shutdown::ALL,
        run::ALL,
        task::ALL,
        question::ALL,
        worker::ALL,
        workspace::ALL,
        router::ALL,
        memory::ALL,
        eval::ALL,
        store::ALL,
        bus::ALL,
        api::ALL,
        kohral::ALL,
        sandbox::ALL,
        budget::ALL,
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    #[test]
    fn names_are_prefixed_unique_and_lowercase() {
        let all = super::all();
        let set: HashSet<_> = all.iter().collect();
        assert_eq!(set.len(), all.len());
        for name in all {
            assert!(name.starts_with(super::PREFIX), "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{name}"
            );
        }
    }
}
