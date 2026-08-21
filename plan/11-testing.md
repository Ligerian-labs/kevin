# 11 — Testing strategy

Kevin must be testable **without a model**. The `fake` worker
([04-workers](./04-workers.md)), an injected `Clock`, a seeded RNG and
replayable event streams make every orchestration path deterministic. Real
CLIs (`claude`, `codex`, `pi`, `opencode`) are exercised only in an opt-in,
cost-capped smoke suite.

## Test pyramid per crate

| Crate | Unit | Property (`proptest`) | Snapshot (`insta`) | Integration | Notes |
|---|---|---|---|---|---|
| `kevin-domain` | aggregate state machines (`Run`, `Task`, `Question`, `Evaluation`, `RouteScore`, `MemoryItem`) with given/when/then helpers; value-object validation; event envelope serde | random command sequences never violate invariants; serde round-trip of every event type; `TaskKind`/`ModelAlias` parsing | event JSON shapes (one snapshot per event type + schema_version) | — | No IO, no tokio. Target ≥ 90 % line coverage. |
| `kevin-config` | layering, env mapping, `deny_unknown_fields`, validation errors collected together | random layer orders produce the documented precedence; any valid config survives `toml` round-trip | effective config of `kevin config show` (redacted) for each profile | — | Fixture configs under `crates/kevin-config/tests/fixtures/*.toml`. |
| `kevin-store` | upcaster registry, request canonicalisation helpers | — | migration list | event store append/OCC/idempotent commands, outbox relay, projection checkpoints, LISTEN/NOTIFY catch-up — **real Postgres** | One schema per test (see below). |
| `kevin-bus` | InProcBus fan-out, lag handling (bounded broadcast) | — | — | PgNotifyBus wake-up + catch-up from store | |
| `kevin-workspace` | strategy selection (`auto`), path containment, `.git/info/exclude` seeding | — | — | git worktree / jj workspace create → use → cleanup on temp repos | Requires `git` and `jj` on PATH; jj tests skip with a clear message if missing. |
| `kevin-worker` | command-line builders per adapter (exact argv), JSONL parsers, usage extraction, effort mapping, policy violations (forbidden flags per tier) | parser never panics on arbitrary lines | argv per adapter × tier × (schema/no schema); parsed `WorkerEvent` streams from golden fixtures | subprocess supervisor against a fake CLI shim: timeout, cancel (SIGTERM→SIGKILL), process-group kill, exit-code classes, bounded buffers under a flooding child | Fixtures `crates/kevin-worker/tests/fixtures/<kind>/*.jsonl`. |
| `kevin-router` | catalog load, cold-start priors, exclusion on retry, `fixed` policy | `RecordRouteOutcome` keeps `alpha,beta ≥ 1`, means finite, scores monotone in quality; Thompson selection with seeded RNG is reproducible | leaderboard table rendering | `routing.*` tables round-trip | |
| `kevin-memory` | ranking formula (cosine + tsvector + decay) with fixed vectors; redaction allowlist | decay never increases importance; search results sorted & bounded by `top_k` | injected context block formatting | pgvector store/search/supersede/forget with `NoopEmbedder`/fixed vectors; `reindex` | `FastEmbedEmbedder` tested once behind `--features fastembed-tests` (downloads model). |
| `kevin-evaluator` | rubric TOML parsing, score normalisation, auto-apply policy gating | — | judge prompts and output schema per rubric | golden judge outputs via fake worker → `evaluation.recorded` + downstream commands | |
| `kevin-orchestrator` | saga reactions table (event → commands), scheduler ready-set, budget arithmetic, failure classification | random DAGs: scheduler respects deps, `max_parallel`, never starts a task twice | plan/understanding prompt contracts | end-to-end runs with fake-worker scenarios (see [05-orchestration](./05-orchestration.md) scenario list); restart/`runtime_restarted`; drain — **real Postgres** | Target ≥ 85 % coverage (domain + orchestrator combined). |
| `kevin-api` | error envelope codes, auth (constant-time), pagination cursors | — | OpenAPI document | `tower::ServiceExt::oneshot` against an app wired to a test store; SSE catch-up with `Last-Event-ID` | |
| `kevin-kohral` | request hash canonicalisation, capabilities JSON, status mapping | identical requests → same hash; any field change → different hash | capabilities & model catalog JSON | ledger projection vs run events; idempotency 202/200/409; `runtime_restarted` on boot; `contract.py` basic/accept-crash/verify-crash against the conformance image | see [08-kohral-runtime](./08-kohral-runtime.md) |
| `kevin-tui` | reducer over API events (pure) | random event orders + snapshots → consistent state; gap → resync requested | screens via `ratatui::backend::TestBackend` (runs list, run detail, inbox modal, plan approval) | — | Never touches the store. |
| `kevin-cli` | clap parsing, exit codes | — | `--help` output per subcommand | `kevin run --no-tui` with embedded runtime + fake worker (smoke) | |

## Aggregate test helpers (`kevin-domain::testing`)

```rust
given(&[run_started(), understanding_completed()])
    .when(ProposePlan { .. })
    .then(&[run.plan_proposed(..)]);          // exact events, ordered
given(..).when(..).then_err(DomainError::InvalidTransition { .. });
```

Helpers: builders for every command/event with sensible defaults,
`arb_run_history()` / `arb_task_history()` strategies for proptest, and an
`assert_invariants(&Run)` checker used by property tests after every step.

## Fixtures and test data layout

```text
crates/<crate>/tests/               # integration tests (cargo convention)
crates/<crate>/tests/fixtures/      # golden files owned by that crate
crates/kevin-worker/tests/fixtures/{claude,codex,pi,opencode}/*.jsonl   # real captured CLI streams, secrets scrubbed
crates/kevin-worker/tests/shim/     # `fake-cli` shell/rust shim placed first on PATH; replays a fixture, honours --sleep/--exit/--flood
crates/kevin-orchestrator/tests/scenarios/*.yaml                        # fake-worker scenarios (04-workers format) — one per 05 scenario name
crates/kevin-evaluator/tests/fixtures/judge/*.json                      # golden judge outputs
crates/kevin-config/tests/fixtures/*.toml
crates/*/tests/snapshots/                                               # insta snapshots, reviewed with `cargo insta review`
testing/                            # workspace-level: docker-compose.test.yml (pgvector), scripts/capture-fixture.sh
```

Fixture capture rule: run the real CLI once with `scripts/capture-fixture.sh
<kind> <prompt>`, which scrubs tokens/keys/paths and stores the stream; record
CLI version in a sidecar `.meta.toml`. A parser change that breaks a fixture is
a real regression until proven otherwise.

## Determinism rules

- **Clock:** `kevin-domain::Clock` trait (`now() -> DateTime<Utc>`); production
  `SystemClock`, tests `FakeClock` (manual advance). Timeouts in the
  orchestrator use `tokio::time` and tests run with `#[tokio::test(start_paused
  = true)]`.
- **Ids:** `IdGen` trait for uuid v7 (`SeqIdGen` in tests yields predictable
  ids so snapshots are stable).
- **Randomness:** router takes `&mut impl rand::Rng`; tests seed `StdRng`.
- **Concurrency:** orchestrator tests assert on *event sets and causal order*,
  not global order, except where a scenario pins `max_parallel_tasks = 1`.
- **Network:** none in unit/integration tests except Postgres (testcontainers)
  and loopback HTTP for API tests. The fastembed model download is feature-gated.
- **Environment:** tests never read `~/.config/kevin`; `KevinConfig` is built
  in-code or from fixtures.

## Postgres in tests

- Default: `testcontainers` starts `pgvector/pgvector:pg16` once per test
  binary (shared container), each test gets its own database created from a
  migrated template (`CREATE DATABASE t_<uuid> TEMPLATE kevin_template`) and
  drops it on success.
- CI override: if `DATABASE_URL` is set (GitHub Actions service container),
  skip testcontainers and use the same per-test database strategy.
- Helper crate `kevin-testkit` (dev-dependency): `TestDb::new().await`,
  `TestApp` (store + bus + orchestrator + API router + fake worker registry),
  `Scenario::load("...")`, `EventAssert` (wait-for-event with timeout, collect
  stream by run_id).

## Worker adapter testing

1. **argv snapshots:** for every adapter × sandbox tier × {with schema, without}
   × {fresh, resume}, snapshot the built command line. Any forbidden flag in a
   non-container tier must be a `WorkerError::PolicyViolation`.
2. **parser goldens:** feed fixture JSONL → assert the `WorkerEvent` sequence
   (snapshot) and the final `Usage`.
3. **supervisor:** shim scenarios — normal exit, non-zero exit, stderr-only
   failure, hang (timeout → SIGTERM → SIGKILL after `kill_grace`), child that
   ignores SIGTERM, child that spawns a grandchild (process-group kill
   verified by pid liveness), stdout flood (reader must apply back-pressure,
   memory bounded), cancellation mid-stream.
4. **doctor:** missing binary, binary present but unauthenticated (shim
   `--auth-fail`), version parse.

## Orchestrator end-to-end (fake worker)

One test per scenario listed in [05-orchestration](./05-orchestration.md)
(happy path, questions answered, question expired with default, plan rejected
then revised, task retry then success, retry exhausted → dependents skipped,
budget exhausted, cancel mid-execution, worker crash → `Transient` retry,
structured-output violation → `Permanent`, input request → question → resume,
restart while running → `runtime_restarted`, drain refuses new runs, headless
auto-approve, Kohral mode defaults for questions). Each asserts the expected
domain-event sequence per aggregate and the final projections.

## API, TUI, CLI

- API: `oneshot` tests for every endpoint (happy + each error code), auth
  failures, idempotent run creation with `Idempotency-Key`, SSE: subscribe,
  disconnect, reconnect with `Last-Event-ID` → no gap/no duplicate.
- TUI: reducer unit tests; `TestBackend` snapshots at fixed 120×40; keybinding
  table test ensures every documented key is bound.
- CLI: `assert_cmd` + `predicates`; `kevin run --no-tui --json` with embedded
  runtime and fake worker emits the expected JSON lines and exit code.

## Kohral conformance

`kevin kohral conformance` builds the conformance image (`workers.fake.enabled
= true`, `profile = "kohral"`), starts the stack, runs
`runtime/conformance/contract.py --runtime hermes` phases `basic`,
`accept-crash` (then kills the gateway container), `verify-crash`. CI job
`kohral-conformance` is required on `main` and on PRs touching `kevin-kohral`,
`kevin-api`, `kevin-orchestrator`, or `deploy/kohral`.

## CI matrix and tooling

| Job | Where | What |
|---|---|---|
| `fmt` / `clippy` | ubuntu | `cargo fmt --check`, `cargo clippy --all-targets --all-features -D warnings` |
| `deny` | ubuntu | `cargo deny check` (licenses, advisories, bans, crate-level dependency rules from 01) |
| `test-linux` | ubuntu + `pgvector/pgvector:pg16` service | `cargo nextest run --workspace` with `DATABASE_URL` |
| `test-macos` | macos-latest | `cargo nextest run --workspace` (testcontainers via Docker if available, else Postgres tests skipped with a visible `SKIPPED` count that must be 0 on linux) |
| `msrv` | ubuntu | `cargo +<MSRV> check --workspace` (MSRV pinned in `rust-toolchain.toml`/`Cargo.toml`; bump deliberately) |
| `coverage` | ubuntu | `cargo llvm-cov nextest` → fail if `kevin-domain` + `kevin-orchestrator` < 85 % lines |
| `kohral-conformance` | ubuntu | see above |
| `smoke-real-clis` | manual / nightly, self-hosted or with secrets | see below, hard cap `budget.default_run_usd = 1.0` |

Toolchain: stable; `cargo nextest` for parallelism and per-test timeouts
(default 60 s, Postgres/e2e 300 s); `insta` snapshots must be committed
(`INSTA_UPDATE=no` in CI).

## Naming and acceptance-criteria mapping

- Unit/property tests: `<unit>_<behaviour>` (e.g. `run_rejects_execution_without_approved_plan`).
- Acceptance criteria from [12-workstreams](./12-workstreams.md): one test per
  criterion named `ac_<ws>_<n>_<slug>` (e.g. `ac_ws05_03_retry_reroutes_to_other_alias`),
  placed in the crate that owns the behaviour. A PR for workstream `ws` must
  contain every `ac_<ws>_*` test, each failing before the implementation and
  passing after (state this in the PR body).
- Scenario files and snapshot names reuse the same slug.

## Definition of done for a workstream PR

1. All `ac_<ws>_*` tests present and green; no `#[ignore]` without a linked issue.
2. `fmt`, `clippy -D warnings`, `deny`, `nextest` (linux + macos), coverage gate green.
3. New event types have snapshots; new endpoints have OpenAPI snapshot + error-code tests.
4. New config keys documented in [03-config-schema](./03-config-schema.md) with validation tests.
5. If it touches workers: argv snapshots for every tier; if it touches Kohral: conformance job green.
6. No real-CLI calls in automated tests.

## Manual smoke checklist (laptop, real CLIs)

Pre-reqs: `kevin workers doctor` all green for the CLIs you have; `kevin db
init`; set `budget.default_run_usd = 1.0`, `budget.max_parallel_tasks = 2`.

1. `kevin run "Add a --version flag to this CLI" --cwd <small throwaway repo>` with `roles.planner = opus5-claude`: understanding appears, ≤ 3 questions, answer them in the TUI, approve plan, 1–3 tasks run on `sonnet5-claude`, a branch/PR is produced, evaluation recorded, cost ≤ $1.
2. Repeat with `routing.kinds.implement.candidates = ["gpt56-codex"]` (codex path), then `sonnet5-pi`, then `sonnet5-opencode`; verify argv in the task log, usage captured (or `null` cost for aliases without prices), transcripts saved.
3. Cancel a run mid-task: subprocess gone (`ps`), `task.cancelled`/`run.cancelled` events, workspace cleaned per `workspace.cleanup`.
4. Kill `kevin serve` during a task, restart: attempt terminalised `runtime_restarted`, run resumable/failed as documented, TUI reconnects via SSE catch-up.
5. `kevin routes` shows updated scores; `kevin lessons` shows ≥ 1 lesson; `kevin proposals ls` shows any proposals; accept one and confirm nothing changed without acceptance.
6. Question expiry: run headless (`--headless`), confirm defaults applied and surfaced in the run summary.
7. Record total spend from `kevin cost`; must be under the cap.

---
Summary: per-crate pyramid (unit → proptest → insta → Postgres integration →
fake-worker e2e → Kohral conformance); determinism via `Clock`/`IdGen`/seeded
RNG; per-test Postgres databases from a template; `ac_<ws>_<n>_<slug>` tests
map workstream acceptance criteria; real CLIs only in a cost-capped manual
smoke suite.
