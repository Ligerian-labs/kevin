# 13 — Roadmap and milestones

Milestones are cumulative; each is demoable. Workstreams (see [12](./12-workstreams.md)) map onto them.

| Milestone | Demo | Workstreams |
|---|---|---|
| **M0 — Skeleton** (week 1) | `cargo build` of the whole workspace; `kevin config show`, `kevin db init/migrate`, `kevin workers doctor` work; CI green with Postgres service. | WS-00 → WS-01, WS-02, WS-03, WS-04, WS-05, WS-07 |
| **M1 — One run, one worker** (weeks 2-3) | `kevin run "add a README badge" --no-tui` uses the fake worker end-to-end: understanding → questions (CLI answer) → plan → one task → integrate → evaluate; all events visible via `kevin runs show`; same path with the real `claude` adapter. | WS-06, WS-08, WS-09, WS-10, WS-11, WS-12 |
| **M2 — Parallel fan-out + TUI** (weeks 4-5) | Multi-task plan executed in parallel worktrees with `codex`/`pi`/`opencode` adapters, routed by Thompson sampling; TUI shows board/log/inbox; PRs opened per task; `kevin routes` leaderboard moves after evaluations. | WS-13, WS-14, WS-15, WS-16, WS-17 |
| **M3 — Daemon + memory** (weeks 6-7) | `kevin serve` on a VPS; `kevin tui --server`; memory retrieval visibly changes planner output on repeated runs; lessons/proposals inbox; metrics + health + drain; release binaries. | WS-18, WS-19, WS-20, WS-21 |
| **M4 — Kohral native** (weeks 8-9) | Kevin image passes `contract.py --runtime hermes` basic + crash phases; Kohral `KevinRuntimeStrategy` provisions a Kevin agent; turn → run → durable status; model catalog from aliases. | WS-22, WS-23, WS-24 |
| **M5 — Hardening** (week 10) | Chaos tests (kill -9 mid-run, db outage), cost caps, docs site, `kevin` homebrew/cargo-binstall. | WS-25 |

## Post-v1 (explicitly out of scope now)

- **WASM tool host** (`kevin-wasm`): wasmtime component model, WIT interfaces for `fs`, `http`, `git` with capability grants per task; ship a first tool (formatter/linter) as a component; explore running the planner/judge *loop* in wasm with host-provided model calls.
- **Direct provider workers**: `AnthropicApiWorker` (Claude API tool runner, structured outputs, prompt caching), OpenAI-compatible worker; enables Kevin without any CLI installed.
- **Embedded Postgres** (`postgresql_embedded`) for zero-setup laptops.
- **Kohral collaboration tools** as MCP server (phase 2 of WS-21) and channel-less conversation resources.
- **Learned router v2**: contextual bandit over task embedding + kind; per-repo priors; cost-aware plan tiering proposals.
- **Multi-operator / tenancy**: only via Kohral.
- **Web UI**: the API is designed for it; not planned.
