# 00 — Vision, scope, and ubiquitous language

> "Everybody needs to have a Kevin in his life."

Kevin is one of the most common first names: a Kevin can do anything — finance,
code, even farming. **Kevin** is an autonomous agent runtime that runs on a
laptop, on a VPS, inside Kohral (see [08-kohral-runtime](./08-kohral-runtime.md)),
and — later — on WASM hosts.

## What Kevin is

1. **A meta-agent.** Given a goal, Kevin first *understands* it with a frontier
   model, *asks* the user what is genuinely ambiguous, *decomposes* the goal into
   a task graph, *routes* each task to the best (worker, model) pair, *executes*
   tasks in parallel on isolated workspaces, *integrates* results, and
   *evaluates* the outcome so the next run is better.
2. **A set of building blocks for existing coding agents.** Kevin does not ship
   its own LLM loop in v1. Its workers are the coding agents already on the
   machine: `claude`, `codex`, `pi`, `opencode` (plus a deterministic `fake`
   worker for tests and Kohral conformance). Every model role Kevin needs
   (planner, clarifier, judge, implementer…) is fulfilled by one of those CLIs.
3. **A secure, event-driven runtime.** Every state change is a domain event
   persisted in Postgres; every child agent is a supervised tokio task with a
   cancellation token, a budget, a timeout, and an isolated workspace.
4. **A memory that improves over time.** Postgres + pgvector stores run
   summaries, lessons, preferences and artifacts; evaluation scores feed a
   per-(task kind, worker, model) routing table so model selection improves with
   use. Evaluations may auto-update routing scores and memory; prompt/config
   changes are only *proposed* to a human.

## What Kevin is not (v1)

- Not a chat bot or a new coding agent — it orchestrates existing ones.
- Not a model gateway — it never talks to provider HTTP APIs directly in v1
  (Worker trait keeps that door open).
- Not a WASM runtime yet — sandboxing relies on the workers' native sandboxes,
  workspace isolation and an allow-listed environment. WASM components for
  tools/agents are a roadmap item ([13-roadmap](./13-roadmap.md)).
- Not multi-tenant. One Kevin instance = one operator (Kohral provides tenancy
  around it).

## Primary user journeys

| # | Journey | Entry point |
|---|---------|-------------|
| J1 | "Kevin, implement feature X in this repo" on a laptop, watch progress in the TUI, answer Kevin's questions, get PR(s). | `kevin run "…"` / `kevin tui` |
| J2 | Kevin runs as a daemon on a VPS; several runs are queued from the CLI/API; questions pile up in an inbox until someone answers. | `kevin serve` + `kevin run --server …` |
| J3 | Kevin is deployed by Kohral as an agent runtime; Kohral sends conversation turns; Kevin treats each turn as a run and streams durable status back. | Kohral → `/v1/runs` |
| J4 | After N runs, an operator inspects the route leaderboard and lessons, and accepts/rejects proposed prompt or config changes. | `kevin routes`, `kevin lessons`, TUI |

## Design principles

- **Modular monolith, explicit boundaries.** Bounded contexts are crates in one
  workspace and one process; contracts are Rust traits + event schemas, so any
  context can be extracted later without domain changes.
- **Events are the spine.** Commands mutate aggregates; aggregates emit events;
  everything else (projections, TUI, API streams, evaluation, Kohral status)
  reacts to events. No component reads another's tables.
- **Safe by default.** Workers run with their own sandbox flags on; dangerous
  bypass flags are only allowed when Kevin itself runs inside a container tier
  that is declared in config.
- **Cheap to run locally, production-grade when deployed.** One binary, one
  Postgres. Structured logs, metrics, health and drain endpoints from day one.
- **Deterministic testability.** The `fake` worker, an injected clock, and
  replayable event streams make every orchestration path testable without a
  model.

## Ubiquitous language (glossary)

| Term | Meaning |
|------|---------|
| **Run** | One end-to-end execution of a user goal. Aggregate root of the Orchestration context. Has a lifecycle, a budget, a task graph, questions, and an evaluation. |
| **Goal** | The user's original prompt/task text plus attachments and the target workspace (repo path). |
| **Understanding** | The planner's structured comprehension of the goal: restated objective, assumptions, risks, open questions, success criteria. |
| **Question** | A clarification Kevin needs from a human before continuing. Has options, a default, a deadline policy. |
| **Plan** | The proposed task graph (tasks + dependencies + acceptance criteria + suggested routes) produced from the understanding and answers. |
| **Task** | A unit of work executed by exactly one worker attempt at a time. Aggregate. Has a *kind*, a *spec*, a *route*, attempts, artifacts. |
| **Task kind** | A value from a fixed taxonomy (`understand`, `clarify`, `plan`, `research`, `implement`, `test`, `review`, `refactor`, `debug`, `write`, `ops`, `evaluate`, `integrate`, `custom:<name>`). Drives routing. |
| **Worker** | An adapter that can execute a task by driving an external coding agent CLI (`claude`, `codex`, `pi`, `opencode`) or the in-process `fake`. |
| **Model** | A provider model identifier as understood by a worker (e.g. `claude-opus-5`, `gpt-5.6`, `anthropic/claude-sonnet-5`). |
| **Model alias** | A config-level name binding `(worker, model, pricing, tier, tags)`, e.g. `opus5-claude`. Routing works on aliases. |
| **Route** | The `(worker, model alias)` chosen for a task attempt. |
| **Route score** | Learned statistics for `(task kind, model alias)`: attempts, successes, mean judge score, cost, latency; used by the router. |
| **Role** | A fixed orchestration responsibility (`planner`, `clarifier`, `judge`, `integrator`) bound to a model alias in config. |
| **Workspace** | An isolated checkout for a task (git worktree or jj workspace) under the run's working area. |
| **Sandbox tier** | `cli-native` (worker's own sandbox), `container` (Kevin itself is containerised; bypass flags allowed), `none` (explicit opt-out). |
| **Artifact** | A file, diff, PR URL, report or structured JSON produced by a task and referenced by id. |
| **Evaluation** | A judge's structured assessment of a task or run against a rubric; emits scores, verdict, lessons, proposals. |
| **Lesson** | A memory item of kind `lesson`: an actionable sentence learned from an evaluation, with provenance. |
| **Memory item** | Anything stored in the RAG store: lesson, preference, fact, run summary, artifact summary. Has an embedding. |
| **Budget** | Limits attached to a run/task: USD, tokens, wall-clock, max attempts, max parallel tasks. |
| **Turn** (Kohral) | One Kohral conversation message. Mapped 1:1 to a Kevin run in Kohral mode. |
| **Event** | Immutable past-tense fact (`TaskSucceeded`) persisted in the event store with an envelope (ids, versions, correlation/causation). |
| **Projection / read model** | A table rebuilt from events for a consumer (TUI board, API, route leaderboard). |
