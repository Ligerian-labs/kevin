# ADR 0002 — Postgres + pgvector as the only store; event-sourced aggregates + projections

**Status:** accepted · **Date:** 2026-08-21

## Context
We need durable runs that survive restarts (Kohral requires durable status + `runtime_restarted` semantics), replayable history for the TUI/API, a RAG memory with vectors, and learned routing tables. The user chose Postgres + pgvector.

## Decision
Single Postgres database (schemas per context). `Run`, `Task`, `Question`, `Evaluation`, `RouteScore`, `MemoryItem` are event-sourced aggregates in `core.events` (per-stream OCC, global position). Projections are rebuilt from events. An outbox + `pg_notify` fan-out wakes other processes; in-process fan-out is a tokio broadcast bus. Worker token streams are *not* domain events; they live in `orch.task_log`.

## Alternatives
- SQLite default + optional Postgres: zero-setup laptop story but two backends and no pgvector/LISTEN.
- State-only tables with an audit log: simpler writes, but replay/resume and Kohral's durability guarantees would need ad-hoc machinery.

## Consequences
`kevin db init` + a compose file for laptops; migrations additive; snapshots for long streams; upcasters for event schema evolution. Embedded Postgres is a possible later convenience (see roadmap).
