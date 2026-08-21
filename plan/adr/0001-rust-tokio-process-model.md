# ADR 0001 — Rust, tokio multi-thread runtime; agents are supervised tasks

**Status:** accepted · **Date:** 2026-08-21

## Context
Kevin must run on a laptop, a VPS and inside Kohral, spawn many concurrent child agents (external CLIs doing network I/O), and stay responsive for a TUI/API. The user asked for "use of threads to spawn agents".

## Decision
One tokio multi-thread runtime per process. A `Run` is owned by a `RunActor` task; each task attempt is a `TaskRunner` task inside the actor's `JoinSet`; subprocesses are `tokio::process` children in their own process group. CPU-bound work (embeddings, future WASM) goes to `spawn_blocking`/a bounded thread pool. Cancellation is a `CancellationToken` tree. Bulkheads are semaphores (global, per worker kind, per run).

## Alternatives
- One OS thread per agent with blocking I/O: simpler mental model, but blocking HTTP/pipes, heavier per agent, poor fit for SSE/TUI.
- Thread-per-agent each with its own current-thread runtime: strong isolation but duplicated runtimes and more plumbing.

## Consequences
Worker adapters must be async and cancellation-safe; tests use a fake worker and an injected clock; we get back-pressure on subprocess pipes for free with bounded readers.
