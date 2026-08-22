# Kevin — implementation plan

> "Everybody needs to have a Kevin in his life."

Kevin is a Rust, event-driven, autonomous agent runtime that orchestrates the
coding agents you already have (`claude`, `codex`, `pi`, `opencode`), routes
each task to the best model, isolates every child agent, remembers what it
learned (Postgres + pgvector) and evaluates itself to improve. It runs on a
laptop, a VPS, and natively inside [Kohral](https://github.com/Ligerian-labs/kohral).

This directory is the **complete plan** meant to be executed by several coding
agents in parallel. Start with `00`–`03` (the contract), then your workstream in
`12`.

| Doc | What it fixes |
|---|---|
| [00-vision](./00-vision.md) | Scope, journeys, principles, **ubiquitous language** |
| [01-architecture](./01-architecture.md) | Bounded contexts, crate map, process model (tokio), event-driven core, storage, topologies |
| [02-domain-model](./02-domain-model.md) | Aggregates, state machines, commands, **event catalog**, envelope, saga, read models |
| [03-config-schema](./03-config-schema.md) | Full TOML schema with defaults, precedence, validation, model catalog, roles, routing |
| [04-workers](./04-workers.md) | `Worker` trait, subprocess supervisor, exact CLI invocations for claude/codex/pi/opencode, fake worker |
| [05-orchestration](./05-orchestration.md) | Services, `RunActor`/saga, phase pipeline with prompt schemas, scheduler, retries, budgets, test scenarios |
| [06-memory-and-learning](./06-memory-and-learning.md) | pgvector memory, embeddings, retrieval; router (Thompson sampling); evaluator, rubrics, auto-apply, proposals |
| [07-api-and-tui](./07-api-and-tui.md) | axum API + SSE, DTOs/error codes, typed client, CLI command tree, ratatui TUI |
| [08-kohral-runtime](./08-kohral-runtime.md) | Kohral runtime contract (Hermes dialect), ledger, image/stack, conformance, Kohral-side strategy |
| [09-security](./09-security.md) | Threat model, sandbox tiers, workspace isolation, env allow-list, redaction, checklist |
| [10-observability-ops](./10-observability-ops.md) | Telemetry, metrics, health/drain, startup/shutdown, runbooks, CI/release |
| [11-testing](./11-testing.md) | Test pyramid, fixtures, determinism, CI matrix, definition of done |
| [12-workstreams](./12-workstreams.md) | **The parallel work packages**: ownership, frozen interfaces, acceptance criteria, waves |
| [13-roadmap](./13-roadmap.md) | Milestones M0–M5 and post-v1 (WASM, direct API workers, …) |
| [adr/](./adr/README.md) | Architecture decision records |

## Decisions already taken (do not re-litigate in a workstream)

- Rust CLI, tokio multi-thread runtime; agents are supervised tasks, workers are subprocesses.
- Workers = external CLIs in v1; Kevin's own roles (planner, judge…) also go through them; no provider API keys in Kevin.
- Postgres + pgvector is the only store; aggregates are event-sourced; projections serve reads.
- Local embeddings (fastembed) by default.
- TOML config, layered, strictly validated.
- Evaluations auto-update routing scores and memory only; prompt/config changes are proposals.
- Kohral compatibility via the Hermes-style contract in an ACL crate; Kohral stack = `kevin-gateway` + `postgres`.
- WASM is deferred (roadmap); sandbox tiers now.

## Conventions for agents

- Read `~/.kb/index.md` triggers (conventional commits, jj, PR workflow) — the repo uses **jj**.
- One workstream = one workspace = one (or a few) PRs; acceptance criteria become `ac_wsNN_*` tests first.
- Frozen interfaces change only via a plan PR (doc + ADR).
- `[inferred — verify]` marks facts to confirm against the installed tool before relying on them. One remains in the plan text ([04](./04-workers.md) §Adapter: claude — `result.structured_output` and the `error_*` result subtypes); per-adapter fixture metadata (`crates/kevin-worker/tests/fixtures/<kind>/inferred.meta.toml`) is the authoritative record of what is still inferred. A live capture settles them.
- `just ci` must be green before a PR; no AI attribution in commits/PRs.
