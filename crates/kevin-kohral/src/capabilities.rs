//! `GET /v1/capabilities` (`plan/08-kohral-runtime.md` §1.4).
//!
//! Kohral decides whether it may run durable conversations against a runtime
//! purely from this document (`HermesRuntimeStrategy::conversationCompatibility`
//! plus `runtime/conformance/contract.py`). The flags are therefore constants,
//! not derived state, and [`REQUIRED_TRUE`] / [`FEATURE_RESTART_FAILURE_CODE`]
//! pin exactly what those two consumers assert.

use serde_json::{Value, json};

/// The failure code Kohral expects on a run that a restart terminalised.
pub const FEATURE_RESTART_FAILURE_CODE: &str = "runtime_restarted";

/// Feature flags Kohral requires to be `true`
/// (`conversationCompatibility` + `contract.py --runtime hermes`).
pub const REQUIRED_TRUE: [&str; 6] = [
    "run_idempotency_persistent",
    "run_status_persistent",
    "run_partial_output",
    "session_resources",
    "runtime_wide_drain",
    "runtime_model_catalog_v1",
];

/// Header Kohral sends to continue a session.
pub const SESSION_CONTINUITY_HEADER: &str = "X-Hermes-Session-Id";
/// Header Kohral sends with the conversation's session key.
pub const SESSION_KEY_HEADER: &str = "X-Hermes-Session-Key";

/// What Kevin advertises. `model` is the `roles.planner` alias — the model a
/// Kohral operator sees as "the agent's model"; `version` is Kevin's semver.
#[must_use]
pub fn document(version: &str, planner_alias: &str, temporary_attachments: bool) -> Value {
    json!({
        "object": "kevin.capabilities",
        "platform": "kevin",
        "version": version,
        "model": planner_alias,
        "auth": {"type": "bearer", "required": true},
        "runtime": {
            "mode": "server_agent",
            "tool_execution": "server",
            "split_runtime": false,
            "description": "Kevin orchestrates coding-agent CLIs inside this workload.",
        },
        "features": features(temporary_attachments),
        "endpoints": endpoints(),
    })
}

/// The `features` object on its own (pinned by a unit test).
#[must_use]
pub fn features(temporary_attachments: bool) -> Value {
    json!({
        "run_submission": true,
        "run_status": true,
        "run_idempotency_persistent": true,
        "run_status_persistent": true,
        "run_partial_output": true,
        "run_restart_failure_code": FEATURE_RESTART_FAILURE_CODE,
        "run_automatic_replay": false,
        "runtime_wide_drain": true,
        "session_resources": true,
        "runtime_model_catalog_v1": true,
        "run_stop": true,
        "run_events_sse": false,
        "run_approval_response": false,
        "temporary_attachments": temporary_attachments,
        "chat_completions": false,
        "chat_completions_streaming": false,
        "responses_api": false,
        "session_chat": false,
        "session_fork": false,
        "skills_api": false,
        "jobs_admin": false,
        "session_continuity_header": SESSION_CONTINUITY_HEADER,
        "session_key_header": SESSION_KEY_HEADER,
    })
}

fn endpoints() -> Value {
    json!({
        "health": {"method": "GET", "path": "/health"},
        "health_detailed": {"method": "GET", "path": "/health/detailed"},
        "capabilities": {"method": "GET", "path": "/v1/capabilities"},
        "models": {"method": "GET", "path": "/v1/kohral/models"},
        "runs": {"method": "POST", "path": "/v1/runs"},
        "run_status": {"method": "GET", "path": "/v1/runs/{run_id}"},
        "run_stop": {"method": "POST", "path": "/v1/runs/{run_id}/stop"},
        "sessions": {"method": "GET", "path": "/api/sessions"},
        "session": {"method": "GET", "path": "/api/sessions/{session_id}"},
        "session_messages": {"method": "GET", "path": "/api/sessions/{session_id}/messages"},
        "drain": {"method": "POST", "path": "/v1/maintenance/drain"},
        "attachments": {
            "method": "PUT",
            "path": "/v1/attachments/{conversation_id}/{message_id}/{attachment_id}",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::{FEATURE_RESTART_FAILURE_CODE, REQUIRED_TRUE, document, features};

    #[test]
    fn kohral_finds_every_flag_it_requires() {
        let features = features(true);
        for flag in REQUIRED_TRUE {
            assert_eq!(features[flag], true, "{flag} must be advertised as true");
        }
        assert_eq!(
            features["run_restart_failure_code"],
            FEATURE_RESTART_FAILURE_CODE
        );
        assert_eq!(features["run_automatic_replay"], false);
    }

    #[test]
    fn kevin_does_not_claim_what_it_does_not_implement() {
        let features = features(false);
        for flag in [
            "chat_completions",
            "chat_completions_streaming",
            "responses_api",
            "session_chat",
            "session_fork",
            "skills_api",
            "jobs_admin",
            "run_events_sse",
            "run_approval_response",
            "temporary_attachments",
        ] {
            assert_eq!(features[flag], false, "{flag} must be advertised as false");
        }
    }

    #[test]
    fn the_document_carries_the_platform_and_version() {
        let doc = document("0.1.0", "opus5-claude", true);
        assert_eq!(doc["object"], "kevin.capabilities");
        assert_eq!(doc["platform"], "kevin");
        assert_eq!(doc["version"], "0.1.0");
        assert_eq!(doc["model"], "opus5-claude");
        assert_eq!(doc["auth"]["type"], "bearer");
        assert_eq!(doc["runtime"]["mode"], "server_agent");
        assert_eq!(doc["endpoints"]["runs"]["path"], "/v1/runs");
    }
}
