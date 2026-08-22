//! Kohral anti-corruption layer (`plan/08-kohral-runtime.md`).
//!
//! Kohral deploys, configures and observes agent runtimes through one durable
//! conversation contract. Kevin speaks its **Hermes dialect** — `POST /v1/runs`
//! with `Idempotency-Key`, a pollable durable status, a runtime model catalog,
//! session resources and a runtime-wide drain — so Kohral can run Kevin as a
//! third `AgentRuntimeStrategy` without a single domain change on its side
//! ([ADR 0008](../../../plan/adr/0008-kohral-hermes-contract.md)).
//!
//! Everything Kohral-shaped stops here. The orchestrator below knows only
//! [`RunMode::Kohral`](kevin_domain::RunMode::Kohral); it has never heard of a
//! turn, an idempotency key or a partial output.
//!
//! # The pieces
//!
//! | Module | What it owns |
//! |---|---|
//! | [`routes`] | the HTTP surface (`plan/08` §1.1) |
//! | [`capabilities`] | the feature flags Kohral gates compatibility on (§1.4) |
//! | [`catalog`] | `/v1/kohral/models`, derived from `[models.*]` (§1.5) |
//! | [`turn`] | turn → [`StartRun`](kevin_domain::run::StartRun) (§1.2) |
//! | [`hash`] | the canonical request hash Hermes and Kevin must agree on |
//! | [`ledger`] | `kohral.runs_ledger`, the durable turn status (§1.3, §2) |
//! | [`metrics`] | `kevin_kohral_*` (`plan/10` §Metrics) |
//! | [`projection`] | the only writer of that ledger after acceptance (§2) |
//! | [`render`] | the Markdown narrative Kohral shows while a turn runs |
//! | [`briefing`] | `AGENTS.md` / `SOUL.md` / documentation pointer (§5.1) |
//! | [`attachments`] | temporary attachments (§1.8) |
//! | [`runtime`] | boot sweep, ledger projection, the router (§1.9) |
//! | [`conformance`] | Kohral's own `contract.py`, run against a real Kevin (§8) |
//!
//! # Wiring it up
//!
//! ```text
//! migrate
//!   └─ kevin_kohral::sweep_runtime_restarted(…)   ← before the orchestrator
//!        └─ Orchestrator::boot(deps)
//!             └─ KohralRuntime::start(KohralDeps { … })
//!                  └─ axum::serve(kohral.bind, runtime.router())
//! ```
//!
//! The briefing is registered the other way round: build it *before*
//! `Orchestrator::boot` and hand it in through `Deps::system_context`.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # fn wire(deps: &mut kevin_orchestrator::orchestrator::Deps, config: &kevin_config::KevinConfig) {
//! let files = kevin_kohral::briefing::BriefingFiles::from_config(&config.kohral);
//! deps.system_context.push(kevin_kohral::briefing::provider(&files));
//! # }
//! ```
//!
//! # What Kevin deliberately does not implement
//!
//! `/v1/chat/completions`, `/v1/responses`, `/api/jobs`, skills, toolsets and
//! the approval round-trip. [`capabilities`] advertises every one of them as
//! `false`, because a runtime that over-claims fails Kohral at run time
//! instead of at rollout time.

pub mod attachments;
pub mod auth;
pub mod briefing;
pub mod capabilities;
pub mod catalog;
pub mod conformance;
pub mod error;
pub mod hash;
pub mod ledger;
pub mod metrics;
pub mod projection;
pub mod render;
pub mod routes;
pub mod runtime;
pub mod state;
pub mod turn;

pub use error::{KohralError, KohralErrorCode, KohralResult};
pub use ledger::{LedgerRow, RunsLedger, TurnStatus};
pub use projection::{KohralLedgerProjection, Narrative};
pub use routes::router;
pub use runtime::{KohralDeps, KohralRuntime, sweep_runtime_restarted};
pub use state::{KohralOptions, KohralState};
