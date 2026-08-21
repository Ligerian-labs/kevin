# ADR 0008 — Kohral compatibility via the Hermes-style runtime contract in an ACL crate

**Status:** accepted · **Date:** 2026-08-21

## Context
Kohral deploys runtimes that satisfy its durable conversation contract (`runtime/conformance/contract.py`): capability discovery, idempotent run submission with `Idempotency-Key`, durable pollable status with monotonic partial output, `runtime_restarted` terminalisation, no automatic replay, model catalog, drain. Two dialects exist: OpenClaw (`/api/kohral/v1/turns`) and Hermes (`/v1/runs`).

## Decision
`kevin-kohral` exposes the Hermes dialect (`/v1/capabilities`, `/v1/runs`, `/v1/kohral/models`, drain, sessions) as an anti-corruption layer: a Kohral turn becomes a Kevin `Run` in mode `Kohral`; a projection maintains `kohral.runs_ledger`. Kohral-side, a `KevinRuntimeStrategy` (type `kevin`) is implemented in the Kohral repo following "Adding a runtime later". Collaboration tools arrive in phase 2 as an MCP server passed to workers.

## Consequences
Kevin's core stays Kohral-agnostic; conformance runs in CI against the Kevin image with the fake worker; Kohral's stack for Kevin = `kevin-gateway` + `postgres`.
