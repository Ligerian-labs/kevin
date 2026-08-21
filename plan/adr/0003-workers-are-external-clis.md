# ADR 0003 — Workers are external coding-agent CLIs behind a `Worker` trait

**Status:** accepted · **Date:** 2026-08-21

## Context
The goal is building blocks *for* existing agents (`claude`, `codex`, `pi`, `opencode`), not a new agent. The user chose to let Kevin's own roles (planner, judge) also run through those CLIs in v1, so Kevin holds no provider API keys.

## Decision
A `Worker` trait (`kevin-worker`) abstracts "run this task attempt with this model and stream events". Adapters: `claude` (`-p --output-format stream-json`), `codex` (`exec --json`), `pi` (`-p --mode json`), `opencode` (`run --format json`), and an in-process `fake` worker for tests/conformance. Structured output uses native schema flags when available. Routing works on config-level model aliases that bind `(worker, model, price, tier)`.

## Alternatives
- Direct provider HTTP clients: more control (streaming, caching, structured outputs, cost) but a second code path to build and secrets to manage.

## Consequences
Kevin inherits each CLI's auth, sandbox and tool set; output formats may drift, so adapters are isolated behind golden fixtures and `kevin workers doctor`. A direct-API worker can be added later without touching orchestration.
