# Security checklist — verification results

The per-workstream checklist at the end of [`plan/09-security.md`](../plan/09-security.md)
says what each crate must verify before its PR. This is the record of walking
it, crate by crate, during WS-25 (hardening). It is a *findings* document: the
plan stays the specification, this says what the code actually does today.

Status values:

| Status | Meaning |
|---|---|
| **verified** | Implemented, and a test asserts the property (not merely exercises the code). |
| **fixed** | Was broken or unenforced; fixed in WS-25, with the test that now proves it. |
| **by construction** | Correct because the code offers no way to do the wrong thing, but nothing would catch a future change. The risk is a silent regression. |
| **gap** | Specified in plan/09 and **not implemented**. Listed with what it would take. |

Every test name below is a real function; run one with
`cargo nextest run -E 'test(<name>)'`.

---

## kevin-config

| Item | Status | Evidence |
|---|---|---|
| Project layer cannot set `sandbox.*`, `workers.*`, `env_passthrough` | verified | `loader.rs` `PROJECT_PROTECTED_SECTIONS` blocks the whole `workers` and `sandbox` tables — stricter than the plan's per-key list, so `extra_args`, `permission_mode`, `sandbox`, `fake.*` and `env_passthrough` are all covered. `ac_ws02_3_project_layer_may_not_touch_protected_sections` asserts the exact error per key. |
| Secrets redacted in `config show` | verified | `redact.rs` masks `*token*`, `*key*`, `*_file` and URL passwords. `ac_ws02_4_redacted_output_hides_secrets_and_names_sources`. |
| Insecure bind rejected | **fixed** | Validation only checked that a *path was configured*, so a non-loopback bind was accepted with a token file that did not exist or was world-readable — the worst case, because the operator believes the port is protected. `token::check_bind_security` now requires the file to exist, be non-empty and be mode `0600`, and `kevin serve` calls it before binding. Kept out of `load()` on purpose: loading stays a pure function of the layers. `ac_ws25_13_1/13_2/13_3`. |

## kevin-worker

| Item | Status | Evidence |
|---|---|---|
| Forbidden flags impossible outside `container` | **fixed** | The matcher compared raw argv tokens, so `codex -c sandbox_mode="danger-full-access"` — which codex parses exactly like the bare form — slipped through: a one-quote bypass of the whole tier. Tokens are now unquoted and `key=value` split before comparison. `ac_ws25_5_2`, `ac_ws25_5_2b` (no false positives). |
| …and the tier is the only switch | **fixed** | `SandboxPolicy::from(&Sandbox)` OR-ed in `sandbox.allow_dangerous_flags`, giving a second, unvalidated way to unlock the bypass flags. It is now derived from the tier alone. `ac_ws25_5_4`. |
| Every adapter checks its **final** argv incl. `extra_args` | verified | claude, codex, pi and opencode all append `extra_args` and then call `check_argv`. `ac_ws06_3`, `ac_ws13_3`, `ac_ws14_3`, `ac_ws15_3`. |
| The two forbidden-flag tables agree | **fixed** | plan/09 says the list "lives in one place"; it lives in two (`kevin_workspace::sandbox::FORBIDDEN_FLAGS`, typed and per-worker, and the flat list in `kevin_worker::policy` that the adapters actually consult, because `kevin-worker` does not depend on `kevin-workspace`). Merging them is a WS-07 refactor; in the meantime `ac_ws25_5_5` fails the build if an entry of the authoritative table is not rejected by the enforced one. |
| Env allow-list applied | verified | `supervisor.rs` `.env_clear().envs(&env)`; the list is names-only from `workers.<kind>.env_passthrough` ∪ `sandbox.env_allowlist_extra` ∪ `KEVIN_*`. `supervisor_applies_env_allowlist_cwd_stdin_and_writes_transcript` asserts a non-listed variable does not reach the child. |
| Secrets never in argv | by construction | No adapter emits a credential flag; provider keys travel as *env names* only. No test asserts it — the argv snapshots would freeze a regression but assert nothing about secrets. |
| Transcripts pass redaction | **fixed** | The supervisor wrote every line to `data_dir` verbatim, so a worker that ran `cat .env` left the credential on disk for the artifact retention window. Lines are now redacted and capped at `TRANSCRIPT_LINE_CAP_BYTES` — a declared constant that nothing used. `ac_ws25_6_2`, `ac_ws25_6_3`. |
| Process-group kill works | verified | `process_group(0)` + `killpg` with SIGTERM → grace → SIGKILL. `ac_ws05_1_cancel_kills_process_group_within_kill_grace` asserts the **grandchild** is dead, which is the load-bearing part. |
| Start failures are classified | **fixed** | Every `worker.start()` error became `Transient`, so a missing binary or a rejected flag burned the task's whole attempt budget and surfaced as "max attempts exhausted" instead of the real cause. `WorkerError::failure_class()` makes `BinaryMissing` / `InvalidAlias` / `PolicyViolation` permanent. `ac_ws25_4_1`, `ac_ws25_4_2`. |

## kevin-workspace

| Item | Status | Evidence |
|---|---|---|
| Worktree / jj isolation | verified | WS-07 acceptance suite (`ac_ws07_*`). |
| Cleanup never deletes outside `workspace.root` | verified | `guard_inside_root` canonicalises and checks containment before any removal. `remove_refuses_paths_outside_root` asserts the victim directory still exists. |
| Out-of-workspace **write detection** | **gap** | Not implemented. plan/09 §Workspace isolation specifies a post-attempt diff of watched paths (`$HOME/.ssh`, `$HOME/.config`, the repo root outside the worktree) failing the attempt `Permanent { reason: "workspace_escape" }` and raising a proposal. The only trace in the tree is an unused event constant `kevin.workspace.escape_detected`. Not attempted here: it is a WS-07 feature (a watched-path snapshot around every attempt), not a hardening tweak, and doing it badly is worse than not doing it — a false positive fails honest work. **Recommended as the next security workstream.** |

## kevin-store / kevin-bus

| Item | Status | Evidence |
|---|---|---|
| Event payloads redacted before append | **fixed** | Nothing redacted them. `core.events` is the most permanent sink in the system and every projection, SSE stream and transcript view is derived from it, so one leaked key was leaked everywhere, forever. `PgEventStore::append` now redacts each payload. `ac_ws25_6_1` reads the raw column back. |
| No secrets in projections | **fixed** (task_log) | `orch.task_log` stores raw worker output (`tool_result` lines) and serves it over the API. Redacted at the single write chokepoint. `ac_ws25_6_4`. The other `orch.*` projections derive from events, which are now redacted upstream. |
| `LISTEN/NOTIFY` payload carries ids only | verified | The payload is the last position, an integer (`event_store.rs`, `pg_notify($1, last_position)`). `ac_ws04_3`. |
| Append is loud when Postgres is gone | **fixed** (test) | Behaviour was already correct; nothing asserted it. `ac_ws25_2_1` (unreachable database → error, never `Ok`), `ac_ws25_2_2` (a backend kill loses and duplicates nothing), `ac_ws25_2_3` (a rejected append writes no rows and no outbox entries). |

## kevin-memory

| Item | Status | Evidence |
|---|---|---|
| Scope enforced in retrieval | verified | `ScopeFilter::for_repo` → `global` + `repo:<hash>`, enforced in SQL. `scopes_isolate_repositories_and_supersede_keeps_only_the_head`. |
| Summariser + redaction before embedding | verified | `MemoryStore::store` vets content with the real `Redactor` **before** embedding or inserting, and refuses with `ContainsSecret` rather than storing a masked version — nothing partially redacted is ever embedded. `ac_ws18_4_redaction_refuses_content_with_an_api_key`. |
| `forget` blanks content, drops the embedding | verified | `SET forgotten_at = now(), content = '', embedding = NULL`; search filters `forgotten_at IS NULL`; emits `memory.item_forgotten`. `ac_ws18_3_forget_removes_the_item_from_search`. |
| Kohral profile forces `repo:<agent-id>` scope | **gap** | Not implemented: nothing in `kevin-kohral` or the serve wiring sets a memory scope, so a Kohral agent uses the ordinary repo-hash scope. Single-agent stacks make this low-severity today (one agent, one store), but it is the mechanism plan/09 relies on if a stack ever hosts two agents. Small fix, in WS-22's area rather than this one. |

## kevin-router / kevin-evaluator

| Item | Status | Evidence |
|---|---|---|
| Judge blind to the model name | verified | A `Scrubber` built from the executor route is applied to *every* prompt section. `ac_ws19_3_...` plants the alias inside a diff and asserts it is absent from the rendered prompt. |
| Auto-apply limited to routing + memory | verified | `AutoApply::apply` has exactly two branches; proposals are counted, never applied. `ac_ws19_2_proposals_are_never_auto_applied`. |
| Proposals need human approval | verified | Same test, plus `ac_ws19_5`. |
| A decision records *why* | **fixed** | `kevin proposals reject --note` printed the note and dropped it. Both decision events are now schema v2 with `note?`, and `Upcasters::domain()` lifts stored v1 payloads. `ac_ws25_11_1`…`11_4`. |

## kevin-orchestrator

| Item | Status | Evidence |
|---|---|---|
| Budgets enforced | **fixed** | The check ran only *after* a worker reported usage, so a run kept admitting attempts until something crossed the limit — and a worker that never reported usage never stopped it. `RunActor::schedule` now consults `budget_spent()` before dispatching. `ac_ws25_7_1`…`7_4` fuzz the bound over random DAGs and per-attempt usage; removing the gate makes `7_1` fail. |
| Cancellation kills subprocesses | verified (worker layer) | Token tree `root → run → attempt` reaches `SpawnOpts.cancel` → `kill_group`. Proven at the worker layer by `ac_ws05_1`; the orchestrator-level `ac_ws08_14_cancel_run_kills_children` uses the in-process fake worker and asserts events only — despite its name it proves nothing about subprocesses. Worth an end-to-end test with a real child. |
| Acceptance criteria only from the plan | by construction | The judge's criteria come from the task-board row (populated from the approved plan); the worker's text enters only as `transcript_summary` and artifacts. No test asserts a worker-authored summary cannot become a criterion. |
| `allow_push` surfaced in the approval view | **fixed** | The plan field existed and the TUI never showed it, so the one flag that widens the blast radius past the workspace was invisible unless the operator read the raw JSON. Now rendered as `· PUSH` on the task row. |

## kevin-api / kevin-cli / kevin-tui

| Item | Status | Evidence |
|---|---|---|
| Bearer auth is constant-time | verified | `subtle::ConstantTimeEq` over SHA-256 digests. `ac_ws16_3_the_token_comparison_is_constant_time`. |
| Token never logged | by construction | Failures log `event=kevin.api.auth_failed, reason=missing\|invalid`; the token is never a field. Deviation from plan/09's wording: there is no `auth=ok` line on success, and the field is `reason`, not `auth`. No test asserts the token's absence from logs. |
| Loopback default | verified | `127.0.0.1:7777` in `default.toml`; asserted by `ac_ws02_5_server_profile_flips_only_its_three_defaults`. |
| Body limit 1 MiB, SSE cap 64, CORS empty | by construction | All three implemented (`MAX_BODY_BYTES`, `MAX_SSE_CONNECTIONS`, `cors_layer` returns `None` for an empty list). **No test** exercises any of the three limits. Cheap to add; left for a WS-16 follow-up rather than widening this PR. |
| `/healthz`+`/readyz` open, `/metrics` off the API bind | verified | `/metrics` is not routed on the API router at all. `ac_ws16_3_health_and_openapi_are_exempt_from_auth`, `ac_ws20_4_metrics_endpoint_exposes_the_documented_names`. |
| Token rotation on SIGHUP | verified | `ac_ws20_5_a_client_attaches_with_a_token_and_rotation_needs_no_downtime`. |
| Startup secrets registered with the redactor | **fixed** | `Redactor::register_secret` existed and was never called, so the pattern list was the *only* defence: a Postgres password that looks like a word, or a hand-written bearer token, was redacted nowhere. `secrets::register` now feeds the database password and every token file at `Backend::open`. Values are kept as `(len, hash)`, never in clear. `ac_ws25_14_1`. |

## kevin-kohral

| Item | Status | Evidence |
|---|---|---|
| Runtime token separate from the operator token | **fixed** (test) | Two verifiers from two files, but only one direction was asserted. A Kohral runtime token is mounted into the agent's stack and reachable by anything inside it, so "it cannot open the operator API" is the half that matters. `ac_ws25_15_1` asserts both directions, plus that an error body never echoes a credential. |
| Identity file never in a worker env | by construction | The path is read only to register the value with the redactor; worker envs are name-allow-listed, so no injection point exists. Note the other half of the plan sentence is unimplemented: `kohral.identity_file` / `collaboration_url` are not yet used to call `KOHRAL_COLLABORATION_URL` at all. |
| Bad token → 401/403 | verified | `ac_ws22_5_bad_token_rejected` across five routes, and a bad-token turn creates no run. |
| No human JWT accepted | by construction | The middleware only compares against the runtime token, so a JWT can never validate — but no test presents a JWT-shaped credential, so the claim is unproven as stated. |
| Overlay protects `server`/`kohral`/`database`/`sandbox`/`workers.*` | **gap** | The Kohral native configuration overlay does not exist yet (plan/08 §Overlay). The project-layer guard is a different mechanism and does not cover it. Whoever implements the overlay must implement the guard in the same PR — an overlay without it hands a Kohral operator the sandbox tier. |

## kevin-telemetry

| Item | Status | Evidence |
|---|---|---|
| Redaction corpus passes | verified | `redact_corpus_matches_golden_outputs` against `redact_corpus.txt`. |
| Bounded record sizes | verified | `FIELD_CAP_BYTES`, `STACK_TRACE_CAP_BYTES`, `RECORD_CAP_BYTES` are all live in `layer.rs`; `TRANSCRIPT_LINE_CAP_BYTES` was dead until WS-25 wired it into the supervisor. `oversized_fields_and_records_are_capped`, `ac_ws25_6_3`. |
| Metrics labels bounded | by construction | Achieved ad hoc — `MatchedPath` route templates, a `method_label` allow-list, const enums in the Kohral metrics. The `metrics::bounded()` helper written for this has **zero production call sites**; it is dead code that reads as a guarantee. Either use it at the label sites or delete it. The end-to-end check (`ac_ws20_4`) only looks for `run_id=` in a scrape, not task or attempt ids. |

---

## Summary

Nine items were broken or unenforced and are fixed here, each with a test:
event-payload redaction, transcript redaction and capping, `orch.task_log`
redaction, the quoted-flag sandbox bypass, `allow_dangerous_flags` as a second
switch, the token-file mode check, startup secret registration, budget
enforcement before dispatch, and `allow_push` visibility. Two more were correct
but untested and now have tests (store behaviour under an outage, Kohral token
separation in both directions).

Four remain open, none of them a WS-25-sized change:

1. **Out-of-workspace write detection** (kevin-workspace) — the largest gap; a
   whole detection mechanism, and the one checklist row with nothing behind it.
2. **The Kohral configuration overlay guard** (kevin-kohral) — must land with
   the overlay itself, not after it.
3. **The Kohral memory scope** (`repo:<agent-id>`) — small, low severity while a
   stack hosts one agent.
4. **Duplicated single sources of truth** — `FORBIDDEN_FLAGS` and
   `EnvAllowlist` each exist twice. The flag tables are now guarded by
   `ac_ws25_5_5`; the second `EnvAllowlist` in `kevin-workspace` is dead code
   and should be deleted.
