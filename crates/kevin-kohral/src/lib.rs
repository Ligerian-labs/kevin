//! Kohral anti-corruption layer (`plan/08-kohral-runtime.md`).
//!
//! Owns the Kohral runtime contract (Hermes dialect): `/v1/capabilities`,
//! `/v1/runs` (+ status, stop), `/v1/kohral/models`, drain, session endpoints,
//! the `kohral.runs_ledger` idempotency ledger, status mapping from `Run*`
//! events, the platform briefing loader and the collaboration client.
//!
//! Dependency direction: depends on `kevin-api` and `kevin-orchestrator`.
//! Implemented by WS-22 (WS-23 adds the image/stack bits).
