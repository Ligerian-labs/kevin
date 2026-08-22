# 10 — Observability and operations

Crate: `kevin-telemetry` (tracing, metrics, redaction, health primitives) +
operational commands in `kevin-cli` (`kevin db …`, `kevin config …`) +
`deploy/` artifacts. Follows the production contract in the KB
(`development/production-software.md`): loggable, observable, failure
resistant, configurable, environment independent, operable.

## Logging (tracing)

- `tracing` + `tracing-subscriber` with an `EnvFilter` from
  `telemetry.log_level`; format `json` (one object per line on stdout) or
  `pretty` when `telemetry.log_format = pretty` (default for a tty /
  `laptop` profile). No application-managed log files; the TUI swallows
  stdout and exposes logs in a pane when running embedded.
- Every record carries: `ts` (UTC RFC3339), `level`, `event` (stable machine
  name), `service = "kevin"`, `version`, `instance = kevin.instance_name`,
  `profile`, plus span fields.
- **Span conventions** (fields, not message text): `run_id`, `task_id`,
  `attempt_id`, `question_id`, `worker` (kind), `model_alias`, `task_kind`,
  `command_id`, `correlation_id` (= run id), `causation_id`, `kohral_turn_id`,
  `session_key`. Spans: `run` → `phase{name}` → `task` → `attempt` →
  `worker_process`; `command{type}`; `projection{name}`; `http{method,route}`.
  Route templates, never raw paths with ids.
- **Stable event names** (prefix `kevin.`):
  `kevin.startup.config_loaded`, `kevin.startup.ready`, `kevin.shutdown.begin`,
  `kevin.shutdown.drained`, `kevin.shutdown.forced`,
  `kevin.run.started|understanding_completed|questions_asked|plan_proposed|plan_approved|executing|integrated|evaluated|completed|failed|cancelled|budget_exhausted`,
  `kevin.task.created|routed|attempt_started|progressed|attempt_succeeded|attempt_failed|retried|skipped|cancelled`,
  `kevin.question.asked|answered|expired`,
  `kevin.worker.spawned|stdout_line|stderr_line|exited|killed|timeout|policy_violation`,
  `kevin.workspace.created|removed|escape_detected`,
  `kevin.router.selected|score_updated`,
  `kevin.memory.stored|retrieved|forgotten|reindexed`,
  `kevin.eval.recorded|proposal_raised|proposal_accepted|proposal_rejected`,
  `kevin.store.appended|version_conflict|outbox_relayed|projection_checkpoint|projection_rebuilt`,
  `kevin.bus.lagged` (broadcast receiver lag), `kevin.api.request`,
  `kevin.api.auth_failed`, `kevin.kohral.turn_accepted|turn_terminal|drain_changed|runtime_restarted`,
  `kevin.sandbox.disabled`, `kevin.budget.warning` (≥80 %).
  `worker.stdout_line/stderr_line` are `debug` and sampled (1 in N after the
  first 200 lines per attempt) — the full transcript lives in `task_log`.
- Severity: `debug` diagnosis; `info` lifecycle/business milestones; `warn`
  recovered degradation (retry, lag, fallback, sandbox disabled); `error`
  failed outcomes needing attention. Exceptions are logged once at the
  owning boundary with `error.class` and `error.message`.
- **Redaction layer** (`kevin-telemetry::redact`, see [09](./09-security.md))
  is a `tracing` `Layer` applied before formatting, and a function used by
  the store/API/memory for payloads. Record size cap 32 KiB, field cap 8 KiB,
  stack trace cap 8 KiB; overflow marked `…[truncated N bytes]`.
- Non-blocking writer (`tracing-appender::non_blocking`) with a bounded
  queue (default 64k lines, `lossy = true` for debug, lossless for ≥ info);
  dropped-line counter exported as `kevin_telemetry_dropped_records_total`.
- `RUST_LOG` overrides `telemetry.log_level` for ad-hoc diagnosis.

## Metrics (Prometheus)

`metrics` facade + `metrics-exporter-prometheus`, served at
`telemetry.metrics_bind` (separate listener; empty = disabled; never on the
API bind). Labels are bounded
enums only — never ids, paths, prompts or error messages. All names are
prefixed `kevin_`.

| Metric | Type | Labels |
|---|---|---|
| `kevin_build_info` | gauge (1) | `version`, `commit`, `profile` |
| `kevin_runs_total` | counter | `mode` (interactive/headless/kohral), `outcome` (completed/failed/cancelled) |
| `kevin_runs_active` | gauge | `status` |
| `kevin_run_duration_seconds` | histogram | `mode`, `outcome` |
| `kevin_run_phase_duration_seconds` | histogram | `phase` |
| `kevin_tasks_total` | counter | `kind`, `outcome` |
| `kevin_tasks_active` | gauge | `kind`, `status` |
| `kevin_task_attempts_total` | counter | `kind`, `worker`, `model_alias`, `outcome` (succeeded/failed/cancelled) |
| `kevin_task_attempt_duration_seconds` | histogram | `kind`, `worker`, `model_alias` |
| `kevin_task_retries_total` | counter | `kind`, `failure_class` |
| `kevin_questions_total` | counter | `outcome` (answered/expired_default/expired_fail) |
| `kevin_question_wait_seconds` | histogram | `mode` |
| `kevin_worker_processes` | gauge | `worker` |
| `kevin_worker_exits_total` | counter | `worker`, `class` (ok/transient/permanent/timeout/killed/policy_violation) |
| `kevin_worker_spawn_duration_seconds` | histogram | `worker` |
| `kevin_worker_semaphore_waiters` | gauge | `worker` |
| `kevin_tokens_total` | counter | `model_alias`, `direction` (input/output/cache_read/cache_write) |
| `kevin_cost_usd_total` | counter (float) | `model_alias`, `role_or_kind` |
| `kevin_budget_exhausted_total` | counter | `dimension` (usd/tokens/wall) |
| `kevin_scheduler_ready_tasks` | gauge | — |
| `kevin_scheduler_blocked_tasks` | gauge | `reason` (deps/semaphore/budget) |
| `kevin_event_store_append_duration_seconds` | histogram | `aggregate_type` |
| `kevin_event_store_version_conflicts_total` | counter | `aggregate_type` |
| `kevin_events_appended_total` | counter | `event_type` |
| `kevin_outbox_backlog` | gauge | — |
| `kevin_outbox_oldest_age_seconds` | gauge | — |
| `kevin_projection_lag_events` | gauge | `projection` |
| `kevin_projection_apply_duration_seconds` | histogram | `projection` |
| `kevin_bus_lagged_total` | counter | `subscriber` |
| `kevin_memory_search_duration_seconds` | histogram | `embedder` |
| `kevin_memory_items` | gauge | `kind`, `scope_type` (global/repo) |
| `kevin_embedding_duration_seconds` | histogram | `embedder` |
| `kevin_router_selections_total` | counter | `kind`, `policy`, `model_alias`, `explored` (true/false) |
| `kevin_eval_overall_score` | histogram | `rubric`, `subject` (run/task) |
| `kevin_eval_proposals_total` | counter | `kind`, `status` |
| `kevin_api_requests_total` | counter | `route`, `method`, `status_class` |
| `kevin_api_request_duration_seconds` | histogram | `route`, `method` |
| `kevin_api_sse_connections` | gauge | — |
| `kevin_kohral_turns_total` | counter | `outcome` (completed/failed/interrupted/runtime_restarted/idempotent_replay/conflict) |
| `kevin_kohral_turns_active` | gauge | — |
| `kevin_kohral_draining` | gauge (0/1) | — |
| `kevin_db_pool_connections` | gauge | `state` (idle/in_use) |
| `kevin_telemetry_dropped_records_total` | counter | `level` |

`kevin_cost_usd_total` is exported as a monotonically incremented **gauge**:
the `metrics` facade has no float counter, `rate()` reads the same either way,
and `orch.cost_ledger` stays the authoritative ledger behind `kevin cost`.

Histogram buckets: durations `0.05..3600 s` log-scaled; scores `0.0..1.0`
step 0.1. Optional OTLP traces via `tracing-opentelemetry` when
`telemetry.otlp_endpoint` is set (spans above carry the same fields; sampled
`parent-based, ratio 1.0` by default, configurable).

## Health and drain

| Endpoint | Meaning | Depends on |
|---|---|---|
| `GET /healthz` | liveness: process is not irrecoverably stuck (event loop responds, supervisor task alive) | nothing external — **never** the database |
| `GET /readyz` | readiness: can accept new runs | db pool connects, migrations at expected version, startup sequence finished, not draining, worker registry initialised |
| `GET /startupz` (optional) | startup still progressing | startup sequence stage |
| `POST/GET/DELETE /api/v1/maintenance/drain` | drain on/off/status | — |

Draining: new run creation returns `503 {code:"draining"}`; running attempts
continue; `readyz` fails; `kevin_kohral_draining = 1`. Kohral's
`/v1/maintenance/drain` (see 08) toggles the same flag. Health responses are
small JSON (`{status, checks:{…}}`), cheap, uncached, and leak no config.

## Startup and shutdown sequences

Startup (each stage logged `kevin.startup.<stage>`; failure before `ready`
exits non-zero with all validation errors printed):

1. Load + validate config (all errors at once; secrets into `SecretString`).
2. Init telemetry (subscriber, metrics exporter, OTLP if set); emit `build_info`.
3. Connect Postgres pool; check `pgvector` extension present.
4. Migrations policy: if `database.auto_migrate` run pending migrations; else
   compare versions and fail readiness (`readyz` reports `migrations_pending`)
   while logging `error` — process stays up for diagnosis, does not accept work.
5. Terminalise stale work: every `task.attempt_started` without terminal event
   → `task.attempt_failed { class: RuntimeRestarted }`; Kohral ledger rows
   non-terminal → `failed / runtime_restarted` (partial output preserved).
6. Rebuild `RunActor`s for non-terminal runs (replay streams); they resume at
   the first unsatisfied saga step (never re-run an attempt automatically).
7. Start projections from their checkpoints; start outbox relay + LISTEN.
8. Start worker registry (`doctor` for enabled workers — missing binary is
   `warn`, the alias is marked unavailable to the router).
9. Bind API (and Kohral listener); flip `ready`; log `kevin.startup.ready`.

Shutdown (SIGTERM/SIGINT, `kevin.shutdown.*`):

1. Flip unready; stop admitting runs/turns (`503 draining`).
2. Stop scheduling new attempts; running attempts get `shutdown_grace_period`
   (default 30 s) to finish; progress keeps being recorded.
3. After grace: cancel tokens → SIGTERM → `workers.kill_grace` (10 s) → SIGKILL process groups;
   record `task.attempt_failed { class: Transient, message: "runtime_shutdown" }`.
4. Flush outbox relay, projections checkpoints, telemetry (bounded 5 s).
5. Stop the event bus, **then** close the pool. Order matters: `PgNotifyBus`
   holds a `LISTEN` connection for as long as its pump runs and
   `PgPool::close()` waits for every connection to be returned, so closing the
   pool first hangs the shutdown forever (`PgNotifyBus::shutdown`,
   `Backend::close`). Exit 0 (or 1 if forced).
A second signal forces immediate step 3.

## Migrations and data

- `sqlx` migrations in `crates/kevin-store/migrations/NNNN_<name>.sql`,
  additive only (expand → backfill → switch → contract across releases);
  never rename/drop a column in the same release that stops writing it.
  Event payload evolution uses `schema_version` + upcasters, never rewriting
  stored events.
- Commands: `kevin db init` (create role/db/extension if privileges allow),
  `kevin db migrate`, `kevin db status` (applied/pending, checksum mismatch),
  `kevin db reset --yes` (dev only; refuses when `profile != laptop`),
  `kevin db rebuild-projection <name|--all>`.
- Adjacent versions must coexist: N and N+1 read the same schema; a migration
  requiring exclusive access is documented in the release notes and gated by
  drain.
- **Backups**: Postgres is authoritative (`pg_dump --format=custom` of the
  database, or platform snapshots); `data_dir` holds rebuildable or
  re-downloadable material (artifacts copies, transcripts, embedding model
  cache). Restore procedure: restore dump → `kevin db status` → `kevin db
  rebuild-projection --all` → start. Exercised in CI monthly via
  `deploy/scripts/backup-restore-test.sh` (dump → restore into scratch db →
  integrity query counts → rebuild projections).
- Retention: `task_log` rows older than `retention.task_log_days` (default 30,
  config key added by this workstream) are pruned by `kevin db prune`; events
  are never pruned in v1.

## Runbooks (symptom → diagnostics → mitigation → verify)

| Symptom | Diagnose | Mitigate | Verify |
|---|---|---|---|
| Run stuck in `executing` | `kevin runs show <id>`; metrics `kevin_scheduler_blocked_tasks`; logs for `run_id` | if blocked on semaphore: raise `budget.max_parallel_tasks`/per-kind; if worker hung: `kevin tasks cancel <task-id>` (attempt killed, retry policy applies) | task board moves; `kevin_tasks_active` drops |
| Run stuck in `awaiting_answers` | `kevin questions ls` inbox | answer via TUI/CLI or `kevin answer --default` | run enters `planning` |
| `run.budget_exhausted` | `kevin cost --run <id>` shows per-task spend | re-run with `--budget-usd`; inspect route leaderboard for expensive aliases | new run completes under budget |
| Worker binary missing / auth broken | `kevin workers doctor` | install/login the CLI; aliases become available on next doctor cycle (every 60 s) or `SIGHUP` | doctor shows `ok` |
| DB down | `readyz` fails with `db`; `kevin_db_pool_connections` 0 | restore db; Kevin retries with backoff; in-flight attempts continue and buffer events in memory up to a bound, then fail `Transient` | `readyz` ok; `kevin_outbox_backlog` drains |
| Projection lag / wrong read model | `kevin_projection_lag_events`; `kevin db status` | `kevin db rebuild-projection <name>` (online; readers see stale until done) | lag 0; spot-check a run |
| Token compromised | API logs `auth_failed` spikes | `kevin config rotate-token`; `SIGHUP`; update clients | old token → 401 |
| Upgrading Kevin | read release notes; `kevin db status` | drain → stop → replace binary/image → `kevin db migrate` (if not auto) → start | `readyz`; smoke run with fake worker |
| Kohral `runtime_restarted` failures | `kevin_kohral_turns_total{outcome="runtime_restarted"}` | expected after crash/redeploy; Kohral retries as a new turn; investigate crash cause in logs | no new occurrences |
| `kevin serve` will not exit on SIGTERM | last log line is `kevin.shutdown.drained` or nothing after it; the process is idle | a second signal forces the cancel path; if it hangs *after* the drain line, something holds a pool connection (the bus' `LISTEN`, a stray listener) — capture a stack dump before `SIGKILL` | the process exits 0 within `shutdown_grace_period` + 5 s |
| Memory growth / embeddings slow | `kevin_memory_items`, `kevin_embedding_duration_seconds` | `kevin memory forget --all-before`, lower `memory.top_k`, raise `concurrency.blocking_threads` | latency histograms back in range |

Alerting (suggested, owner = operator): page on `readyz` failing > 5 min,
`kevin_outbox_oldest_age_seconds` > 300, zero `RunActor` progress with active
runs > 30 min; ticket on projection lag, rising `worker_exits_total{class="transient"}`,
`telemetry_dropped_records_total`.

## Release process

- Semver; conventional commits drive the changelog (`git-cliff`); `CHANGELOG.md`
  per release; tags `vX.Y.Z`.
- CI (GitHub Actions) on every PR: `cargo fmt --check`, `cargo clippy
  --all-targets -D warnings`, `cargo deny check`, `cargo test --workspace`
  with a Postgres+pgvector service container (`pgvector/pgvector:pg16`),
  doc build, `kevin config validate` on sample configs; nightly `cargo audit`.
- Conformance job: build the Kohral image, run `kevin kohral conformance`
  (basic + accept-crash/verify-crash with the fake worker) — required for
  release tags.
- Release workflow: `cargo-dist` (or equivalent) builds `x86_64/aarch64` for
  linux-gnu/musl and macOS, attaches checksums + SBOM; container image built
  multi-arch with provenance + SBOM, signed (cosign), published by digest;
  release notes list migrations and any drain requirement.
- Every deployment logs `kevin.startup.ready` with `version`/`commit` and
  `kevin_build_info` changes, so regressions correlate with rollouts.

---
**Summary:** metrics are prefixed `kevin_` with bounded enum labels only;
`/healthz` is liveness and never touches the database; `/readyz` requires
db + migrations + startup complete + not draining; drain is one flag shared
by the API and the Kohral listener; startup terminalises stale attempts as
`runtime_restarted` and rebuilds `RunActor`s before accepting work.
