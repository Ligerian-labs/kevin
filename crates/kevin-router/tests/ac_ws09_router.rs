//! WS-09 acceptance criteria (`plan/12-workstreams.md` §WS-09), in-memory half:
//!
//! 1. `fixed` policy is deterministic
//! 2. Thompson with a seeded RNG is reproducible
//! 3. after 50 outcomes favouring alias A, A is selected ≥ 80 % of the time
//! 4. the exclusion list is honoured on retry
//! 5. cost is computed from prices and null when they are unknown
//!
//! (6) `kevin routes` / `explain` lives in `crates/kevin-cli/tests/ac_ws09_routes.rs`
//! and the Postgres-backed half in `tests/ac_ws09_pg.rs`.

use std::sync::Arc;

use chrono::Utc;
use kevin_config::{KevinConfig, RoutingPolicy};
use kevin_domain::route_score::{BetaPrior, RecordRouteOutcome};
use kevin_domain::{Complexity, Decimal, Effort, FailureClass, ModelAlias, TaskKind, Usage};
use kevin_router::{
    InMemoryRouteScoreRepo, Policy, Router, RoutingError, SeededRngSource, SelectRouteQuery,
};
use rust_decimal::prelude::FromPrimitive;

fn alias(name: &str) -> ModelAlias {
    ModelAlias::new(name).expect("valid alias")
}

/// Default config with `codex` first so the learned winner is *not* the alias
/// the tier preference would favour anyway.
fn config(policy: RoutingPolicy) -> KevinConfig {
    let mut config = KevinConfig::default();
    config.routing.policy = policy;
    config
        .routing
        .kinds
        .get_mut(&TaskKind::Implement)
        .expect("implement candidates")
        .candidates = vec![
        alias("gpt56-codex"),
        alias("sonnet5-claude"),
        alias("opus5-claude"),
    ];
    config
}

fn seeded_router(policy: RoutingPolicy, seed: u64) -> (Router, Arc<InMemoryRouteScoreRepo>) {
    let repo = Arc::new(InMemoryRouteScoreRepo::new());
    let router = Router::from_config(&config(policy), repo.clone())
        .with_rng(Arc::new(SeededRngSource::new(seed)));
    (router, repo)
}

fn outcome(alias_name: &str, success: bool) -> RecordRouteOutcome {
    RecordRouteOutcome {
        task_kind: TaskKind::Implement,
        alias: alias(alias_name),
        success,
        quality: Some(if success { 0.9 } else { 0.2 }),
        cost_usd: None,
        wall_ms: 60_000,
        failure_class: (!success).then_some(FailureClass::Permanent),
        recorded_at: Utc::now(),
        prior: BetaPrior::UNIFORM,
    }
}

// ---------------------------------------------------------------------------
// (1) fixed policy is deterministic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws09_1_fixed_policy_is_deterministic() {
    let (router, _repo) = seeded_router(RoutingPolicy::Fixed, 1);
    for _ in 0..20 {
        let selection = router
            .select(SelectRouteQuery::new(TaskKind::Implement))
            .await
            .expect("route");
        assert_eq!(selection.policy, Policy::Fixed);
        assert!(!selection.explored, "fixed never explores");
        assert_eq!(
            selection.alias(),
            &alias("gpt56-codex"),
            "fixed always takes the first configured candidate"
        );
        assert_eq!(selection.route.worker.as_str(), "codex");
        assert_eq!(selection.route.effort, Some(Effort::High));
    }

    // Even after outcomes that would move a learning policy, `fixed` does not move.
    let (router, repo) = seeded_router(RoutingPolicy::Fixed, 1);
    for _ in 0..10 {
        router
            .record_outcome(outcome("gpt56-codex", false))
            .await
            .expect("record");
    }
    assert_eq!(
        repo.get(&TaskKind::Implement, &alias("gpt56-codex"))
            .expect("row")
            .attempts,
        10
    );
    let selection = router
        .select(SelectRouteQuery::new(TaskKind::Implement))
        .await
        .expect("route");
    assert_eq!(selection.alias(), &alias("gpt56-codex"));
}

// ---------------------------------------------------------------------------
// (2) Thompson with a seeded RNG is reproducible
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws09_2_thompson_is_reproducible_with_a_seed() {
    async fn run(seed: u64) -> Vec<(String, f32)> {
        let (router, _repo) = seeded_router(RoutingPolicy::Thompson, seed);
        router
            .record_outcome(outcome("sonnet5-claude", true))
            .await
            .expect("record");
        let mut out = Vec::new();
        for _ in 0..10 {
            let selection = router
                .select(SelectRouteQuery::new(TaskKind::Implement))
                .await
                .expect("route");
            let selected = selection.selected().expect("a selected candidate");
            out.push((selected.alias.to_string(), selected.sampled_success));
        }
        out
    }

    let first = run(42).await;
    let second = run(42).await;
    assert_eq!(first.len(), 10);
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.0, b.0, "same seed must pick the same alias");
        assert!(
            (a.1 - b.1).abs() < f32::EPSILON,
            "same seed must draw the same sample: {} vs {}",
            a.1,
            b.1
        );
    }
    assert!(
        first.iter().any(|(a, _)| a != &first[0].0) || run(7).await != first,
        "a different seed must be able to produce a different sequence"
    );

    // A per-query seed pins one selection regardless of the router's source.
    let (router, _repo) = seeded_router(RoutingPolicy::Thompson, 999);
    let pinned = SelectRouteQuery::new(TaskKind::Implement).rng_seed(5);
    let a = router.select(pinned.clone()).await.expect("route");
    let b = router.select(pinned).await.expect("route");
    assert_eq!(a.alias(), b.alias());
    assert_eq!(a.candidates, b.candidates);
}

// ---------------------------------------------------------------------------
// (3) 50 outcomes favouring A → A selected ≥ 80 %
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws09_3_learning_favours_the_winning_alias() {
    let (router, _repo) = seeded_router(RoutingPolicy::Thompson, 2024);
    for _ in 0..50 {
        router
            .record_outcome(outcome("gpt56-codex", true))
            .await
            .expect("record A");
        router
            .record_outcome(outcome("sonnet5-claude", false))
            .await
            .expect("record B");
    }

    let mut winner = 0;
    let runs = 100;
    for _ in 0..runs {
        let selection = router
            .select(SelectRouteQuery::new(TaskKind::Implement))
            .await
            .expect("route");
        if selection.alias() == &alias("gpt56-codex") {
            winner += 1;
        }
    }
    assert!(
        winner >= 80,
        "alias A selected {winner}/{runs} times, expected at least 80"
    );
    assert!(
        winner < runs,
        "the exploration floor must still try the unsampled candidate"
    );
}

// ---------------------------------------------------------------------------
// (4) exclusion list honoured on retry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws09_4_exclusion_is_honoured_on_retry() {
    let (router, _repo) = seeded_router(RoutingPolicy::Thompson, 11);
    let first = router
        .select(SelectRouteQuery::new(TaskKind::Implement))
        .await
        .expect("route");
    let failed = first.alias().clone();

    for seed in 0..25 {
        let retry = router
            .select(
                SelectRouteQuery::new(TaskKind::Implement)
                    .exclude([failed.clone()])
                    .rng_seed(seed),
            )
            .await
            .expect("retry route");
        assert_ne!(
            retry.alias(),
            &failed,
            "excluded alias must never be picked"
        );
        let excluded = retry
            .candidates
            .iter()
            .find(|c| c.alias == failed)
            .expect("excluded candidate is still reported");
        assert_eq!(
            excluded.excluded_reason.as_deref(),
            Some("excluded by the caller (retry exclusion)")
        );
    }

    // Excluding everything, including `[roles].default`, leaves no route.
    let all = vec![
        alias("gpt56-codex"),
        alias("sonnet5-claude"),
        alias("opus5-claude"),
    ];
    let err = router
        .select(SelectRouteQuery::new(TaskKind::Implement).exclude(all))
        .await
        .expect_err("no route");
    match err {
        RoutingError::NoRoute { task_kind, reason } => {
            assert_eq!(task_kind, TaskKind::Implement);
            assert!(reason.contains("retry exclusion"), "reason: {reason}");
        }
        other => panic!("expected NoRoute, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (5) cost from prices, null when unknown
// ---------------------------------------------------------------------------

#[test]
fn ac_ws09_5_cost_is_computed_from_prices_and_null_when_unknown() {
    let config = KevinConfig::default();
    let router = Router::from_config(&config, Arc::new(InMemoryRouteScoreRepo::new()));
    let prices = router.prices();
    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 200_000,
        ..Usage::ZERO
    };

    // sonnet5-claude: $3/M in, $15/M out → 3 + 3 = $6.
    assert_eq!(
        prices.cost(&alias("sonnet5-claude"), &usage),
        Some(Decimal::from(6))
    );
    // haiku45-claude: $1/M in, $5/M out → 1 + 1 = $2.
    assert_eq!(
        prices.cost(&alias("haiku45-claude"), &usage),
        Some(Decimal::from(2))
    );
    // gpt56-codex has no prices in the catalog → cost is null, never zero.
    assert_eq!(prices.cost(&alias("gpt56-codex"), &usage), None);
    assert_eq!(prices.cost(&alias("does-not-exist"), &usage), None);

    // A cost the worker itself reported always wins.
    let reported = Usage {
        cost_usd: Some(Decimal::from_f64(0.42).expect("decimal")),
        ..usage
    };
    assert_eq!(
        prices.effective_cost(&alias("gpt56-codex"), &reported),
        Decimal::from_f64(0.42)
    );
}

// ---------------------------------------------------------------------------
// Supporting behaviour of the same workstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn role_kinds_resolve_through_roles_not_routing() {
    let (router, _repo) = seeded_router(RoutingPolicy::Thompson, 3);
    for (kind, expected, effort) in [
        (TaskKind::Understand, "opus5-claude", Effort::XHigh),
        (TaskKind::Plan, "opus5-claude", Effort::XHigh),
        (TaskKind::Clarify, "opus5-claude", Effort::High),
        (TaskKind::Evaluate, "opus5-claude", Effort::High),
        (TaskKind::Integrate, "sonnet5-claude", Effort::Medium),
    ] {
        let selection = router
            .select(SelectRouteQuery::new(kind.clone()).complexity(Complexity::High))
            .await
            .expect("route");
        assert_eq!(selection.alias(), &alias(expected), "kind {kind}");
        assert_eq!(selection.policy, Policy::Fallback);
        assert_eq!(selection.candidates.len(), 1);
        if kind == TaskKind::Clarify {
            // `[roles.effort]` has no clarifier entry → complexity default.
            assert_eq!(selection.route.effort, Some(Effort::XHigh));
        } else {
            assert_eq!(selection.route.effort, Some(effort), "kind {kind}");
        }
    }
}

#[tokio::test]
async fn kinds_without_candidates_fall_back_to_the_default_role() {
    let (router, _repo) = seeded_router(RoutingPolicy::Thompson, 4);
    let selection = router
        .select(SelectRouteQuery::new(TaskKind::Research).complexity(Complexity::Low))
        .await
        .expect("route");
    assert_ne!(
        selection.policy,
        Policy::Fallback,
        "research has candidates"
    );

    let selection = router
        .select(SelectRouteQuery::new(
            TaskKind::custom("data-migration").expect("kind"),
        ))
        .await
        .expect("route");
    assert_eq!(selection.policy, Policy::Fallback);
    assert_eq!(selection.alias(), &alias("sonnet5-claude"));
}

#[tokio::test]
async fn disabled_workers_and_budget_filter_candidates() {
    let mut config = config(RoutingPolicy::Thompson);
    config.workers.codex.enabled = false;
    let repo = Arc::new(InMemoryRouteScoreRepo::new());
    let router =
        Router::from_config(&config, repo.clone()).with_rng(Arc::new(SeededRngSource::new(5)));

    let selection = router
        .select(SelectRouteQuery::new(TaskKind::Implement))
        .await
        .expect("route");
    assert_ne!(selection.alias(), &alias("gpt56-codex"));
    let disabled = selection
        .candidates
        .iter()
        .find(|c| c.alias == alias("gpt56-codex"))
        .expect("candidate listed");
    assert_eq!(
        disabled.excluded_reason.as_deref(),
        Some("worker `codex` is disabled")
    );

    // A route whose mean cost exceeds what is left of the budget is dropped.
    let mut expensive = outcome("sonnet5-claude", true);
    expensive.cost_usd = Some(Decimal::from(5));
    router.record_outcome(expensive).await.expect("record");
    let selection = router
        .select(
            SelectRouteQuery::new(TaskKind::Implement)
                .budget_left_usd(Decimal::from_f64(0.5).expect("decimal")),
        )
        .await
        .expect("route");
    assert_eq!(selection.alias(), &alias("opus5-claude"));
    let over = selection
        .candidates
        .iter()
        .find(|c| c.alias == alias("sonnet5-claude"))
        .expect("candidate listed");
    assert!(
        over.excluded_reason
            .as_deref()
            .is_some_and(|r| r.contains("remaining budget")),
        "reason: {:?}",
        over.excluded_reason
    );
}

#[tokio::test]
async fn epsilon_greedy_uses_the_empirical_win_rate() {
    let (router, _repo) = seeded_router(RoutingPolicy::EpsilonGreedy, 8);
    for _ in 0..10 {
        router
            .record_outcome(outcome("gpt56-codex", true))
            .await
            .expect("record");
        router
            .record_outcome(outcome("sonnet5-claude", false))
            .await
            .expect("record");
        router
            .record_outcome(outcome("opus5-claude", false))
            .await
            .expect("record");
    }
    let selection = router
        .select(SelectRouteQuery::new(TaskKind::Implement))
        .await
        .expect("route");
    assert_eq!(selection.policy, Policy::EpsilonGreedy);
    assert_eq!(selection.alias(), &alias("gpt56-codex"));
    let winner = selection.selected().expect("selected");
    assert!(
        (winner.sampled_success - 1.0).abs() < f32::EPSILON,
        "win rate should be 1.0, got {}",
        winner.sampled_success
    );
}

#[tokio::test]
async fn reset_puts_a_route_back_on_its_tier_prior() {
    let (router, repo) = seeded_router(RoutingPolicy::Thompson, 6);
    router
        .record_outcome(outcome("sonnet5-claude", true))
        .await
        .expect("record");
    router
        .record_outcome(outcome("gpt56-codex", false))
        .await
        .expect("record");

    let updates = router
        .reset(Some(&TaskKind::Implement), Some(&alias("gpt56-codex")))
        .await
        .expect("reset");
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert!(update.reset);
    assert_eq!(update.alias, alias("gpt56-codex"));
    // gpt56-codex is a frontier alias → Beta(3, 1).
    assert!((update.stats.alpha - 3.0).abs() < f32::EPSILON);
    assert!((update.stats.beta - 1.0).abs() < f32::EPSILON);
    assert_eq!(update.stats.attempts, 0);
    assert_eq!(
        repo.get(&TaskKind::Implement, &alias("sonnet5-claude"))
            .expect("untouched")
            .attempts,
        1,
        "reset with an alias filter leaves other rows alone"
    );

    let all = router.reset(None, None).await.expect("reset all");
    assert_eq!(all.len(), 2);
}
