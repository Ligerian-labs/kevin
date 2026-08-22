//! WS-25 security checklist — the cross-crate rows of `plan/09-security.md`.
//!
//! Two properties that belong to no single crate, so nothing was asserting
//! them:
//!
//! - **the two forbidden-flag tables must agree.** `plan/09` §Sandbox tiers
//!   says "the forbidden flag list lives in one place". It lives in two:
//!   `kevin_workspace::sandbox::FORBIDDEN_FLAGS` (the authoritative,
//!   worker-aware table) and `kevin_worker::policy::FORBIDDEN_FLAGS` (the
//!   token list the adapters actually check, because `kevin-worker` does not
//!   depend on `kevin-workspace`). A flag added to one and not the other is a
//!   silent hole, and this crate depends on both, so it is where the two can
//!   be compared.
//! - **`orch.task_log` is a redaction sink.** It holds raw worker output —
//!   `assistant`, `tool_call` and `tool_result` lines — and feeds the API and
//!   the SSE stream, so a credential in a tool result would be served to every
//!   client and kept for the retention window.

mod common;

use std::sync::Arc;

use kevin_orchestrator::projections::{NewTaskLogLine, TaskLog};
use kevin_store::PgEventStore;
use kevin_testkit::pg::TestDb;
use kevin_worker::SandboxPolicy;
use kevin_workspace::sandbox::{FORBIDDEN_FLAGS, ForbiddenFlagShape};

// ---------------------------------------------------------------------------
// The two forbidden-flag tables agree
// ---------------------------------------------------------------------------

/// Every entry of the authoritative table is rejected by the policy the worker
/// adapters actually consult, under the `cli-native` tier.
#[test]
fn ac_ws25_5_5_every_authoritative_forbidden_flag_is_rejected_by_the_worker_policy() {
    let native = SandboxPolicy::cli_native();
    assert!(
        !FORBIDDEN_FLAGS.is_empty(),
        "the authoritative table is empty; this test would pass vacuously"
    );
    for entry in FORBIDDEN_FLAGS {
        // Both spellings a CLI accepts: `--opt value` and `--opt=value`.
        let argvs: Vec<Vec<String>> = match entry.shape {
            ForbiddenFlagShape::Flag(flag) => {
                vec![vec![entry.worker.to_string(), flag.to_owned()]]
            }
            ForbiddenFlagShape::OptionValue { option, value } => vec![
                vec![
                    entry.worker.to_string(),
                    option.to_owned(),
                    value.to_owned(),
                ],
                vec![entry.worker.to_string(), format!("{option}={value}")],
            ],
        };
        for argv in argvs {
            assert!(
                native.check_argv(&argv).is_err(),
                "`{}` is forbidden by kevin-workspace but kevin-worker accepts {argv:?}; \
                 add it to kevin_worker::policy::FORBIDDEN_FLAGS",
                entry.render(),
            );
            assert!(
                SandboxPolicy::container().check_argv(&argv).is_ok(),
                "the container tier must still allow {argv:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// orch.task_log is a redaction sink
// ---------------------------------------------------------------------------

/// A tool result containing a credential must not be stored in `orch.task_log`
/// — it is served over the API and kept for the retention window.
#[tokio::test]
async fn ac_ws25_6_4_task_log_payloads_are_redacted_before_they_are_stored() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let _store = Arc::new(PgEventStore::new(db.pool().clone()));
    let log = TaskLog::new(db.pool().clone());

    let secret = "ghp_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    let task_id = kevin_domain::TaskId::new();
    log.append(&NewTaskLogLine::new(
        task_id,
        1,
        "tool_result",
        serde_json::json!({
            "tool": "shell",
            "output": format!("GITHUB_TOKEN={secret}\nremote set"),
            "nested": { "env": [format!("Bearer {secret}")] },
        }),
    ))
    .await
    .expect("append a task log line");

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM orch.task_log WHERE task_id = $1")
            .bind(task_id.as_uuid())
            .fetch_one(db.pool())
            .await
            .expect("read orch.task_log");
    let text = stored.to_string();
    assert!(
        !text.contains(secret),
        "the token survived into orch.task_log: {text}"
    );
    assert!(
        kevin_telemetry::redact::contains_marker(&text),
        "nothing was redacted at all: {text}"
    );
    // The rest of the line is untouched, so the log stays useful.
    assert_eq!(stored["tool"], "shell");
    db.close().await;
}
