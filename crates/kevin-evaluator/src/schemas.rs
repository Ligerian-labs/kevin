//! The JSON schema the judge answers with, loaded from
//! `crates/kevin-evaluator/schemas/kevin.evaluation.v1.json`.
//!
//! The document mirrors `plan/06-memory-and-learning.md` §3.2 exactly. The
//! generic schema accepts any criterion name; [`evaluation_for`] specialises it
//! for one rubric so a missing or invented criterion is a schema violation the
//! judge runner repairs in one turn instead of a silent zero.

use std::sync::LazyLock;

use serde_json::Value;

use crate::rubric::Rubric;

/// `kevin.evaluation.v1` as JSON text.
pub const EVALUATION_V1_JSON: &str = include_str!("../schemas/kevin.evaluation.v1.json");

/// `$id` of the judge schema.
pub const EVALUATION_V1_ID: &str = "kevin.evaluation.v1";

static EVALUATION_V1: LazyLock<Value> = LazyLock::new(|| {
    let value: Value =
        serde_json::from_str(EVALUATION_V1_JSON).expect("kevin.evaluation.v1 is valid JSON");
    debug_assert_eq!(value["$id"], Value::String(EVALUATION_V1_ID.to_owned()));
    value
});

/// The parsed schema (compiled once).
#[must_use]
pub fn evaluation() -> &'static Value {
    &EVALUATION_V1
}

/// The schema with `scores[].criterion` restricted to `rubric`'s keys and
/// `scores` sized to the rubric, so every criterion must be scored exactly once.
#[must_use]
pub fn evaluation_for(rubric: &Rubric) -> Value {
    let mut schema = evaluation().clone();
    let keys: Vec<Value> = rubric.keys().into_iter().map(Value::from).collect();
    let len = Value::from(keys.len());
    schema["properties"]["scores"]["items"]["properties"]["criterion"] =
        serde_json::json!({ "type": "string", "enum": keys });
    schema["properties"]["scores"]["minItems"] = len.clone();
    schema["properties"]["scores"]["maxItems"] = len;
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_parses_carries_its_id_and_compiles() {
        assert_eq!(
            evaluation()["$id"],
            Value::String(EVALUATION_V1_ID.to_owned())
        );
        jsonschema::validator_for(evaluation()).expect("compiles");
    }

    #[test]
    fn the_rubric_specialised_schema_pins_the_criteria() {
        let rubric = Rubric::builtin("code").unwrap();
        let schema = evaluation_for(&rubric);
        jsonschema::validator_for(&schema).expect("compiles");
        assert_eq!(schema["properties"]["scores"]["minItems"], 6);
        assert_eq!(schema["properties"]["scores"]["maxItems"], 6);
        let enum_keys = schema["properties"]["scores"]["items"]["properties"]["criterion"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(enum_keys, rubric.keys());
    }
}
