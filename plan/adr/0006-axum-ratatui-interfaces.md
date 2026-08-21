# ADR 0006 — axum API + SSE; ratatui TUI as a pure API client

**Status:** accepted · **Date:** 2026-08-21

## Decision
`kevin-api` (axum, utoipa OpenAPI, Bearer token, loopback by default) is the only interface to the runtime. Live updates are SSE streams of event envelopes with `Last-Event-ID` catch-up from the store. `kevin-tui` (ratatui) and `kevin run` talk to the API through `kevin-api::client`; when no server is configured they embed the runtime in-process and bind an ephemeral port. Kohral's contract is an additional router mounted by `kevin-kohral`.

## Alternatives
TUI reading the database directly (breaks context ownership and remote use); WebSocket instead of SSE (bidirectional not needed; SSE is simpler to proxy and resume).

## Consequences
One client code path for laptop and VPS; the TUI is testable against a fake API.
