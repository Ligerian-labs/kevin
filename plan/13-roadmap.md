# 13 — Roadmap and milestones

Milestones are cumulative; each is demoable. Workstreams (see [12](./12-workstreams.md)) map onto them.
**All six milestones are delivered**; the "Shipped" column records what actually
landed, and the exceptions are listed under post-v1 below.

| Milestone | Demo | Workstreams | Shipped |
|---|---|---|---|
| **M0 — Skeleton** (week 1) | `cargo build` of the whole workspace; `kevin config show`, `kevin db init/migrate`, `kevin workers doctor` work; CI green with Postgres service. | WS-00 → WS-01, WS-02, WS-03, WS-04, WS-05, WS-07 | ✅ as specified |
| **M1 — One run, one worker** (weeks 2-3) | `kevin run "add a README badge" --no-tui` uses the fake worker end-to-end: understanding → questions (CLI answer) → plan → one task → integrate → evaluate; all events visible via `kevin runs show`; same path with the real `claude` adapter. | WS-06, WS-08, WS-09, WS-10, WS-11, WS-12 | ✅ as specified (the `claude` path behind `KEVIN_LIVE_TESTS=1`) |
| **M2 — Parallel fan-out + TUI** (weeks 4-5) | Multi-task plan executed in parallel worktrees with `codex`/`pi`/`opencode` adapters, routed by Thompson sampling; TUI shows board/log/inbox; PRs opened per task; `kevin routes` leaderboard moves after evaluations. | WS-13, WS-14, WS-15, WS-16, WS-17 | ✅ as specified |
| **M3 — Daemon + memory** (weeks 6-7) | `kevin serve` on a VPS; `kevin tui --server`; memory retrieval visibly changes planner output on repeated runs; lessons/proposals inbox; metrics + health + drain; release binaries. | WS-18, WS-19, WS-20, WS-21 | ✅ as specified |
| **M4 — Kohral native** (weeks 8-9) | Kevin image passes `contract.py --runtime hermes` basic + crash phases; Kohral `KevinRuntimeStrategy` provisions a Kevin agent; turn → run → durable status; model catalog from aliases. | WS-22, WS-23, WS-24 | ✅ contract, image and strategy; collaboration ([08 §4](./08-kohral-runtime.md)) stayed phase 2 |
| **M5 — Hardening** (week 10) | Chaos tests (kill -9 mid-run, db outage), cost caps, docs site, `kevin` homebrew/cargo-binstall. | WS-25 | ⚠️ chaos/load/cost-cap tests and the security-checklist walk shipped; **docs site and homebrew/cargo-binstall did not** |

## Post-v1 (explicitly out of scope now)

Added after the v1 program closed:

- **Docs site** and **Homebrew tap / `cargo-binstall` metadata** (deferred from
  M5). Kevin is installed from source or from the release archives today; see
  [`docs/releasing.md`](../docs/releasing.md), which also records the decision
  not to publish the library crates to crates.io.
- **Four security gaps** left open by the WS-25 checklist walk, listed in
  [09 §Security checklist](./09-security.md) and
  [`docs/security-checklist.md`](../docs/security-checklist.md).
- **Run-level `role_overrides.judge`** — accepted by `StartRun` but not honoured
  by the evaluator ([05 §Runs](./05-orchestration.md)).

- **WASM tool host** (`kevin-wasm`): wasmtime component model, WIT interfaces for `fs`, `http`, `git` with capability grants per task; ship a first tool (formatter/linter) as a component; explore running the planner/judge *loop* in wasm with host-provided model calls.
- **Direct provider workers**: `AnthropicApiWorker` (Claude API tool runner, structured outputs, prompt caching), OpenAI-compatible worker; enables Kevin without any CLI installed.
- **Embedded Postgres** (`postgresql_embedded`) for zero-setup laptops.
- **Kohral collaboration tools** as MCP server ([08 §4](./08-kohral-runtime.md) phase 2, deferred out of WS-22/WS-23: no `kevin-kohral::collaboration`, no `kohral.collaboration_requests` table and no `kevin mcp collaboration` shipped; only `kohral.collaboration_url` exists in the config) and channel-less conversation resources.
- **Learned router v2**: contextual bandit over task embedding + kind; per-repo priors; cost-aware plan tiering proposals.
- **Multi-operator / tenancy**: only via Kohral.
- **Web UI**: the API is designed for it; not planned.
