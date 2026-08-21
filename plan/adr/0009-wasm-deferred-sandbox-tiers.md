# ADR 0009 — WASM deferred; sandbox tiers now

**Status:** accepted · **Date:** 2026-08-21

## Context
The long-term idea is WASM-hosted tools/agents. The user asked to ignore that for now.

## Decision
v1 security relies on: workers' native sandboxes (`cli-native` tier), workspace isolation, env allow-list, redaction, and a `container` tier where Kevin itself is isolated (Kohral stack) so bypass flags are acceptable. `Worker`, `Sandbox` and a future `ToolHost` trait are the extension seams for wasmtime components and OS sandboxes.

## Consequences
No wasm dependency in v1; the roadmap keeps a spike (`kevin-wasm`) for component-model tools with WIT-scoped capabilities.
