//! HTTP API interface crate (`plan/07-api-and-tui.md` §1–2).
//!
//! Owns the axum router under `/api/v1` (REST + SSE with `Last-Event-ID`),
//! bearer auth, the error envelope and stable error codes, DTOs, OpenAPI
//! (utoipa), health/readiness/drain endpoints, and the typed `KevinClient`
//! behind the `client` feature (no axum dependency) used by the TUI and CLI.
//!
//! Features: `server` (default) pulls in `kevin-orchestrator` and axum;
//! `client` builds only the typed client + DTOs.
//!
//! Dependency direction: depends on `kevin-orchestrator` (server) and
//! `kevin-domain`; `kevin-tui` depends only on the `client` feature.
//! Implemented by WS-16 (WS-20 adds drain/metrics wiring).

#[cfg(feature = "client")]
pub mod client {
    //! Typed HTTP client (`KevinClient`) — implemented by WS-16.
}
