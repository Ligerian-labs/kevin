//! The JSON schemas Kevin's own roles ask for, loaded from
//! `crates/kevin-orchestrator/schemas/*.json`.
//!
//! `kevin.understanding.v1` and `kevin.plan.v1` are frozen: they mirror the
//! documents in `plan/05-orchestration.md` §3.2/§3.4 and the serde shape of
//! [`Understanding`](kevin_domain::Understanding) / [`Plan`](kevin_domain::Plan)
//! exactly, so a domain value serialises straight into a valid document.
//! `kevin.questions.v1`, `kevin.integration.v1` and `kevin.summary.v1` cover
//! the clarifier, the integrator and the summariser
//! (`plan/06-memory-and-learning.md` §1.5).
//!
//! Note that `Plan.suggested_route` is written by the orchestrator *after* a
//! plan is parsed; the planner never emits it and the schema (which forbids
//! additional properties) therefore does not list it.

use std::sync::LazyLock;

use serde_json::Value;

/// `kevin.understanding.v1` as JSON text.
pub const UNDERSTANDING_V1_JSON: &str = include_str!("../../schemas/kevin.understanding.v1.json");
/// `kevin.plan.v1` as JSON text.
pub const PLAN_V1_JSON: &str = include_str!("../../schemas/kevin.plan.v1.json");
/// `kevin.questions.v1` as JSON text.
pub const QUESTIONS_V1_JSON: &str = include_str!("../../schemas/kevin.questions.v1.json");
/// `kevin.integration.v1` as JSON text.
pub const INTEGRATION_V1_JSON: &str = include_str!("../../schemas/kevin.integration.v1.json");
/// `kevin.summary.v1` as JSON text.
pub const SUMMARY_V1_JSON: &str = include_str!("../../schemas/kevin.summary.v1.json");

/// `$id` of the understanding schema.
pub const UNDERSTANDING_V1_ID: &str = kevin_domain::understanding::UNDERSTANDING_SCHEMA_ID;
/// `$id` of the plan schema.
pub const PLAN_V1_ID: &str = kevin_domain::plan::PLAN_SCHEMA_ID;
/// `$id` of the clarifier schema.
pub const QUESTIONS_V1_ID: &str = "kevin.questions.v1";
/// `$id` of the integration schema.
pub const INTEGRATION_V1_ID: &str = "kevin.integration.v1";
/// `$id` of the summariser schema.
pub const SUMMARY_V1_ID: &str = "kevin.summary.v1";

macro_rules! schema {
    ($fn_name:ident, $static_name:ident, $json:ident, $id:ident) => {
        static $static_name: LazyLock<Value> = LazyLock::new(|| {
            let value: Value =
                serde_json::from_str($json).expect(concat!(stringify!($json), " is valid JSON"));
            debug_assert_eq!(value["$id"], Value::String($id.to_owned()));
            value
        });

        /// The parsed schema (compiled once).
        #[must_use]
        pub fn $fn_name() -> &'static Value {
            &$static_name
        }
    };
}

schema!(
    understanding,
    UNDERSTANDING_V1,
    UNDERSTANDING_V1_JSON,
    UNDERSTANDING_V1_ID
);
schema!(plan, PLAN_V1, PLAN_V1_JSON, PLAN_V1_ID);
schema!(questions, QUESTIONS_V1, QUESTIONS_V1_JSON, QUESTIONS_V1_ID);
schema!(
    integration,
    INTEGRATION_V1,
    INTEGRATION_V1_JSON,
    INTEGRATION_V1_ID
);
schema!(summary, SUMMARY_V1, SUMMARY_V1_JSON, SUMMARY_V1_ID);

/// The plan schema with `tasks.maxItems` lowered to `max_tasks`
/// (`orchestrator.max_tasks_per_run`); never raised above the frozen 24.
#[must_use]
pub fn plan_with_max_tasks(max_tasks: usize) -> Value {
    let mut schema = plan().clone();
    let frozen = kevin_domain::plan::DEFAULT_MAX_TASKS;
    let max = max_tasks.clamp(1, frozen);
    schema["properties"]["tasks"]["maxItems"] = Value::from(max);
    schema
}

/// Every schema, `(id, document)`, in a stable order.
#[must_use]
pub fn all() -> Vec<(&'static str, &'static Value)> {
    vec![
        (UNDERSTANDING_V1_ID, understanding()),
        (PLAN_V1_ID, plan()),
        (QUESTIONS_V1_ID, questions()),
        (INTEGRATION_V1_ID, integration()),
        (SUMMARY_V1_ID, summary()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_parses_carries_its_id_and_compiles() {
        for (id, schema) in all() {
            assert_eq!(schema["$id"], Value::String(id.to_owned()), "{id}");
            jsonschema::validator_for(schema).unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }

    #[test]
    fn plan_max_items_follows_the_configured_limit() {
        assert_eq!(plan_with_max_tasks(6)["properties"]["tasks"]["maxItems"], 6);
        assert_eq!(
            plan_with_max_tasks(99)["properties"]["tasks"]["maxItems"],
            24
        );
        assert_eq!(plan_with_max_tasks(0)["properties"]["tasks"]["maxItems"], 1);
    }
}
