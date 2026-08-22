//! WS-25 security checklist — the Kohral boundary, both directions.
//!
//! `plan/09-security.md` §Kohral boundary: the runtime port and the operator
//! API are two surfaces with two credentials, and Kevin "never accepts human
//! JWTs". WS-22's `serve_kohral_serves_the_runtime_contract` proves one half —
//! the operator token is refused on the Kohral port. The other half was never
//! asserted: a leaked *Kohral runtime token* (mounted by the platform into the
//! agent's stack, and therefore reachable by anything inside it) must not open
//! the operator API, which can start runs, answer questions and read every
//! transcript.
//!
//! Needs Postgres; skips cleanly without it.

mod common;

use std::time::Duration;

use common::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_ws25_15_1_the_kohral_token_and_the_operator_token_are_not_interchangeable() {
    kevin_testkit::skip_unless_pg!();
    let harness = Harness::with_scenario(common::SCENARIO).await;
    let daemon = harness.serve(&["--kohral", "--bind", "127.0.0.1:0"]).await;
    let kohral = daemon
        .kohral_url()
        .expect("--kohral binds the runtime contract")
        .to_owned();
    let api = daemon.api().to_owned();
    let client = reqwest::Client::new();

    assert_ne!(
        harness.token(),
        harness.kohral_token(),
        "the fixture must use two different credentials or this proves nothing"
    );

    // Each token works on its own surface …
    let ok = client
        .get(format!("{kohral}/v1/capabilities"))
        .bearer_auth(harness.kohral_token())
        .send()
        .await
        .expect("GET /v1/capabilities");
    assert_eq!(ok.status(), 200, "the Kohral token opens the Kohral port");

    let ok = client
        .get(format!("{api}/api/v1/runs"))
        .bearer_auth(harness.token())
        .send()
        .await
        .expect("GET /api/v1/runs");
    assert_eq!(ok.status(), 200, "the API token opens the operator API");

    // … and on no other. This is the direction WS-22 left untested: anything
    // inside the agent's Kohral stack can read the runtime token.
    for path in ["/api/v1/runs", "/api/v1/config", "/api/v1/proposals"] {
        let refused = client
            .get(format!("{api}{path}"))
            .bearer_auth(harness.kohral_token())
            .send()
            .await
            .unwrap_or_else(|e| panic!("GET {path}: {e}"));
        assert_eq!(
            refused.status(),
            401,
            "the Kohral runtime token must not open {path}"
        );
    }

    let refused = client
        .get(format!("{kohral}/v1/capabilities"))
        .bearer_auth(harness.token())
        .send()
        .await
        .expect("GET /v1/capabilities with the API token");
    assert!(
        matches!(refused.status().as_u16(), 401 | 403),
        "the operator token must not open the Kohral port, got {}",
        refused.status()
    );

    // The unauthenticated probes stay unauthenticated on the operator API and
    // are not a way in (`plan/09` §API authentication).
    let (status, _) = daemon.get("/healthz").await;
    assert_eq!(status, 200);
    let (status, body) = daemon.get("/api/v1/runs").await;
    assert_eq!(status, 401, "no token at all is refused");
    assert!(
        !body.contains(harness.token()) && !body.contains(harness.kohral_token()),
        "an error body must never echo a credential: {body}"
    );

    daemon.signal("TERM");
    daemon.wait(Duration::from_secs(60)).await;
    harness.close().await;
}
