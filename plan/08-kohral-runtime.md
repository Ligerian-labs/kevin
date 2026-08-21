# 08 — Kohral runtime integration

Crate: `kevin-kohral` (anti-corruption layer over `kevin-api`/`kevin-orchestrator`),
plus `deploy/kohral/`. Goal: Kohral deploys, configures and observes Kevin
exactly like it does OpenClaw and Hermes — **Kevin becomes a third
`AgentRuntimeStrategy` type (`kevin`) with no Kohral domain changes**
(Kohral `docs/07-agent-runtimes.md`, "Adding a runtime later").

Kohral facts this chapter depends on (read from `~/workspace/kohral`, commit of
2026-08-20): `docs/07-agent-runtimes.md`, `docs/10-conversations.md`,
`docs/10-agent-collaboration.md`, `runtime/README.md`,
`runtime/conformance/contract.py`, `runtime/hermes/patches/0001-durable-runs.patch`,
`0003-runtime-model-catalog.patch`, `runtime/hermes/overlay/kohral_run_store.py`,
`src/AgentRuntime/Domain/AgentRuntimeStrategy.php`,
`src/AgentRuntime/Infrastructure/HermesRuntimeStrategy.php`,
`src/AgentRuntime/Application/ModelCatalog.php`.

## 1. Contract choice: the Hermes-style surface

Kohral knows two durable conversation contracts: OpenClaw's
(`PUT /api/kohral/v1/turns/{turnId}`, camelCase) and Hermes'
(`POST /v1/runs` + `Idempotency-Key`, snake_case). Kevin implements the
**Hermes-style** one because:

- it is header-based idempotency on a plain `POST`, which maps 1:1 onto Kevin's
  `StartRun{command_id = Idempotency-Key}`;
- its status vocabulary (`queued/running/stopping/completed/failed/cancelled`)
  is the one Kohral's `HermesRuntimeStrategy::turnStatus()` already normalises;
- it carries the model catalog endpoint (`/v1/kohral/models`) and the
  session-resources endpoints Kohral polls;
- the Kohral-side strategy can be a thin copy of `HermesRuntimeStrategy` (§7).

Kevin does **not** implement `/v1/chat/completions`, `/v1/responses`,
`/api/jobs`, skills or toolsets. `GET /v1/capabilities` advertises exactly what
exists.

### 1.1 Endpoint table (served by `kevin serve --kohral` on `kohral.bind`, default `0.0.0.0:8080`)

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/health` and `/v1/health` | none | liveness: `{"status":"ok","platform":"kevin","version":"<semver>"}` |
| GET | `/health/detailed` | bearer | readiness + counters (db ok, active runs, draining, uptime_s, version) — feeds `metrics()` |
| GET | `/v1/capabilities` | bearer | contract discovery (§1.4) |
| GET | `/v1/kohral/models` | bearer | runtime model catalog v1 (§1.5) |
| POST | `/v1/runs` | bearer | submit a turn (§1.2) |
| GET | `/v1/runs/{run_id}` | bearer | durable status (§1.3) |
| POST | `/v1/runs/{run_id}/stop` | bearer | idempotent interrupt |
| GET | `/v1/runs/{run_id}/events` | bearer | SSE of `kevin.run.*` events (optional, `run_events_sse=true` only when implemented) |
| GET | `/api/sessions` | bearer | session resources (§1.6) |
| GET | `/api/sessions/{session_id}` | bearer | one session |
| GET | `/api/sessions/{session_id}/messages` | bearer | messages of a session |
| POST / GET / DELETE | `/v1/maintenance/drain` | bearer | runtime-wide drain (§1.7) |
| PUT / DELETE | `/v1/attachments/{conversation_id}/{message_id}/{attachment_id}` | bearer | temporary attachments (§1.8) |

Auth: `Authorization: Bearer <token>`; token read once from `kohral.token_file`
(`/run/secrets/kohral-runtime-token`, Kohral secret binding
`KEVIN_RUNTIME_TOKEN → API_SERVER_KEY`), constant-time compare. Failure →
`401 {"error":{"message":"Invalid API key","type":"invalid_request_error","code":"invalid_api_key"}}`
(the conformance script accepts 401 or 403). All other errors use the Hermes
shape `{"error":{"message","type","code"}}` **and** a top-level `"code"` so both
Kohral parsers find it.

### 1.2 `POST /v1/runs` — turn submission

Headers: `Idempotency-Key` (required; Kohral sends its turn UUID; regex
`^[A-Za-z0-9][A-Za-z0-9._:-]{0,255}$`), `X-Hermes-Session-Key` (Kohral sends
`kohral:<conversationId>`; optional).

Body (as sent by `HermesRuntimeStrategy::submitTurn`):

```json
{ "input": "<user message>",
  "instructions": "<system entries of the history joined by blank lines>",
  "conversation_history": [{"role":"user|assistant","content":"..."}],
  "session_id": "<conversationId>",
  "model": "hermes-agent | <provider/model override>",
  "attachments": [] }
```

Mapping to Kevin:

| Kohral field | Kevin |
|---|---|
| `Idempotency-Key` | `CommandId` of `StartRun` **and** `kohral.runs_ledger.idempotency_key`; `RunId` is a fresh uuid v7 returned as `run_id`. |
| `input` | `Goal.text` |
| `instructions` | prepended to the planner/worker system context as "Operator instructions" (contains Kohral's per-turn control-plane context) |
| `conversation_history` | injected as "Conversation so far" context block, capped at the last 100 messages / 200 KB (Kohral already caps); also stored on the run for `/api/sessions/*/messages` |
| `session_id` / `X-Hermes-Session-Key` | `Run.mode = Kohral{ turn_id, session_key, session_id }`; used to group runs into sessions |
| `model` | `"hermes-agent"`/empty → no override; `provider/model` → resolved to a model alias (§1.5) and applied as a **role override** for `planner`, `judge` and the routing `default`; unknown → `400 code=unknown_model` |
| `attachments` | list of `{path,size,sha256}` returned by §1.8 → `Goal.attachments` |

Acceptance is **committed before execution starts**: in one Postgres
transaction Kevin (a) looks up the ledger by key, (b) inserts the ledger row
with `request_hash`, (c) appends `run.started`. Only then does the `RunActor`
spawn. Responses:

| Situation | Status | Body |
|---|---|---|
| new key | `202` | run status object (§1.3) with `status:"queued"` |
| same key, same hash | `200` | current status object of the existing run |
| same key, different hash | `409` | `{"code":"idempotency_conflict","error":{...}}` |
| draining and key unknown | `503` | `{"code":"gateway_draining"}` |
| missing/invalid key | `400` | `{"code":"invalid_idempotency_key"}` |

`request_hash = sha256(json_canonical({"body": body, "session_key": header}))`
with sorted keys, `(",",":")` separators, UTF-8, no NaN — byte-compatible with
`kohral_run_store.canonical_request_hash` so the same turn retried by Kohral
always matches.

### 1.3 `GET /v1/runs/{run_id}` — durable status

```json
{ "object": "kevin.run",
  "run_id": "…", "status": "queued|running|stopping|completed|failed|cancelled",
  "partial_output": "…append-only…", "seq": 7,
  "message_id": "msg_<uuid>",            // stable per run, generated at acceptance
  "output": "…",                         // only when status == completed (== partial_output)
  "usage": {"input_tokens": 0, "output_tokens": 0, "cache_read_tokens": 0, "cost_usd": 0.0},
  "session_id": "…", "model": "…",
  "error_code": "runtime_restarted",     // only when failed; ^[a-z][a-z0-9_]{1,63}$
  "error": "diagnostic text",            // only when failed
  "last_event": "run.completed",
  "created_at": 1755770000.0, "updated_at": 1755770012.5 }
```

Rules (mirrors Kohral's "Turn invariants"):

- `seq` is monotonic: `+1` on every `partial_output` append and `+1` on every
  terminal transition. A backwards `seq` or rewritten prefix is a Kohral
  `runtime_protocol_error`, so `partial_output` is **append-only**; the final
  output is reconciled with the same algorithm as Hermes
  (`reconcile_completed_output`: keep the streamed checkpoint, append only the
  unseen suffix).
- Terminal statuses: `completed`, `failed`, `cancelled`. Kohral maps
  `cancelled → interrupted`, `running|stopping → running`.
- Status objects are read from `kohral.runs_ledger` (never from in-memory
  state) so they survive process restarts; `404 code=run_not_found` otherwise.
- Status never depends on a browser or HTTP connection; polling is the contract.

What Kevin writes into `partial_output` for a turn: a human-readable, Markdown
progress narrative — understanding summary, the assumptions Kevin made instead
of asking (§3), one line per task transition, integration result — and, on
completion, the final answer (integrator summary + artifact links/PR URLs).

### 1.4 `GET /v1/capabilities`

```json
{ "object": "kevin.capabilities", "platform": "kevin", "version": "<kevin semver>",
  "model": "<roles.planner alias>",
  "auth": {"type": "bearer", "required": true},
  "runtime": {"mode": "server_agent", "tool_execution": "server", "split_runtime": false,
              "description": "Kevin orchestrates coding-agent CLIs inside this workload."},
  "features": {
    "run_submission": true, "run_status": true,
    "run_idempotency_persistent": true, "run_status_persistent": true, "run_partial_output": true,
    "run_restart_failure_code": "runtime_restarted", "run_automatic_replay": false,
    "runtime_wide_drain": true, "session_resources": true, "runtime_model_catalog_v1": true,
    "run_stop": true, "run_events_sse": false, "run_approval_response": false,
    "temporary_attachments": true,
    "chat_completions": false, "chat_completions_streaming": false, "responses_api": false,
    "session_chat": false, "session_fork": false, "skills_api": false, "jobs_admin": false,
    "session_continuity_header": "X-Hermes-Session-Id", "session_key_header": "X-Hermes-Session-Key"
  },
  "endpoints": {"health": {"method":"GET","path":"/health"}, "runs": {"method":"POST","path":"/v1/runs"}, "...": "..."} }
```

Kohral's `conversationCompatibility()` requires **all** of
`run_idempotency_persistent`, `run_status_persistent`, `run_partial_output`,
`session_resources`, `runtime_wide_drain` = true,
`run_restart_failure_code = "runtime_restarted"`, `run_automatic_replay = false`;
`contract.py` additionally asserts `runtime_model_catalog_v1 = true`. Flags are
constants in `kevin-kohral`; a unit test pins them.

### 1.5 `GET /v1/kohral/models`

```json
{ "object": "kohral.runtime_model_catalog", "version": 1,
  "providers": [
    { "id": "anthropic", "name": "Anthropic (via claude CLI)",
      "models": [ {"id": "claude-opus-5", "name": "opus5-claude", "capabilities": ["reasoning"]},
                  {"id": "claude-sonnet-5", "name": "sonnet5-claude", "capabilities": ["reasoning"]} ] },
    { "id": "openai", "name": "OpenAI (via codex CLI)",
      "models": [ {"id": "gpt-5.6", "name": "gpt56-codex", "capabilities": ["reasoning"]} ] } ] }
```

Derived from `[models.*]`: provider id = alias `provider` key when present
(pi), else a per-worker default (`claude → anthropic`, `codex → openai`,
`opencode → prefix before '/'`); ids match `[a-z0-9][a-z0-9._-]*`, model ids
`[A-Za-z0-9][A-Za-z0-9._:/-]*`, ≤ 2000 models; only aliases whose worker
`doctor()` reports authenticated are listed (Hermes lists authenticated
providers only). `capabilities` contains `"reasoning"` for tiers
`frontier|balanced`; Kohral adds `"tools"` itself. `name` is optional for
Kohral (`ModelCatalog.php` reads `id`, `name`, `capabilities`) — `[inferred —
verify]` that Kohral does not reject extra keys. Reverse mapping for
`POST /v1/runs.model = "<provider>/<model>"` → first alias with that
`(provider, model)`.

### 1.6 Session resources

A Kohral *conversation* = Kevin *session* keyed by `session_id` (one run per
turn). Kohral's `sessions()` reads `payload['sessions'] ?? payload` and then
`/api/sessions/{id}/messages` (`payload['messages'] ?? payload`), so Kevin
returns both the Hermes list envelope and the key Kohral prefers:

```json
GET /api/sessions            → {"object":"list","sessions":[{"id":"<session_id>","session_id":"<session_id>","title":"…","created_at":…,"updated_at":…,"message_count":4}],"data":[…same…],"limit":100,"offset":0,"has_more":false}
GET /api/sessions/{id}       → {"id":"…","session_id":"…","runs":["<run_id>",…],"created_at":…,"updated_at":…}
GET /api/sessions/{id}/messages → {"object":"list","session_id":"…","messages":[{"id":"msg_…","role":"user|assistant","content":"…","created_at":…,"run_id":"…"}],"data":[…same…]}
```

Messages have stable ids (`message_id` of the run for the assistant message;
`umsg_<run_id>` for the user message) so Kohral's reconciliation never
duplicates. Source: `kohral.runs_ledger` + `kohral.session_messages` (§2).

### 1.7 Drain

`POST /v1/maintenance/drain` sets `draining=true` (new keys → 503
`gateway_draining`, existing keys still resolve), `GET` reports, `DELETE`
clears. Payload `{"draining": bool, "accepting": bool, "active_runs": n}`
(`accepting = !draining`; field names `[inferred — verify]` against
`HermesRuntimeStrategy::parseDrainState`). Drain also flips `/health/detailed`
→ `"drainable": true` and the orchestrator's admission gate (05-orchestration).

### 1.8 Temporary attachments

Kohral uploads raw bytes with `PUT`, headers `X-Kohral-Filename` (base64url),
`X-Kohral-Sha256`, `Content-Length`; expects `{"path": "/tmp/kohral-uploads/…", "size": int, "sha256": "<64 hex>"}`
(path **must** start with `/tmp/kohral-uploads/`). Kevin stores under
`/tmp/kohral-uploads/<conversation>/<message>/<attachment>--<name>` on an
ephemeral tmpfs, verifies sha256, enforces `kohral.max_attachment_bytes`
(default 25 MiB), `DELETE` removes; ids validated as safe identifiers. Files
are passed to workers as read-only inputs. If this ships later, advertise
`temporary_attachments: false` until then.

### 1.9 Boot, crash and replay semantics

- On startup, before binding the port: `UPDATE kohral.runs_ledger SET status='failed', error_code='runtime_restarted', seq=seq+1 WHERE status IN ('queued','running','stopping')` and the matching `run.failed{class: RuntimeRestarted}` events are appended for those runs (the orchestrator does the same terminalisation for its own aggregates; the ledger projection and this UPDATE are reconciled by run id). Partial output is preserved.
- Kevin never resumes or replays accepted work after a crash.
- `POST /v1/runs/{id}/stop`: terminal → `200` with current object; otherwise
  ledger `status='stopping'`, `CancelRun` issued, response
  `{"run_id":…,"status":"stopping"}`; repeated stops are no-ops. The run
  terminalises as `cancelled` (Kohral shows `interrupted`) with the partial
  output kept.
- Conformance hooks (only when `workers.fake.enabled = true`, i.e. the
  conformance profile): input `[[KOHRAL_HOLD]]` → fake worker hangs until
  cancelled/killed (accept-crash phase), input `reply deterministically` →
  completes with output exactly `kohral-ok`. The production image ships with
  `workers.fake.enabled = false`; conformance runs the image with
  `KEVIN__WORKERS__FAKE__ENABLED=true` and `KEVIN__ROLES__PLANNER=fake` etc.
  (the config fragment `deploy/kohral/conformance.toml`, loaded through `KEVIN_CONFIG`;
  `kevin.profile` stays `kohral`).

## 2. Ledger and projections (schema `kohral`)

```sql
CREATE TABLE kohral.runs_ledger (
  idempotency_key text PRIMARY KEY,
  request_hash    char(64) NOT NULL,
  request_json    jsonb NOT NULL,
  run_id          uuid NOT NULL UNIQUE,
  session_id      text NOT NULL,
  session_key     text,
  model           text,
  status          text NOT NULL CHECK (status IN ('queued','running','stopping','completed','failed','cancelled')),
  partial_output  text NOT NULL DEFAULT '',
  seq             bigint NOT NULL DEFAULT 0 CHECK (seq >= 0),
  message_id      text NOT NULL,
  usage           jsonb NOT NULL DEFAULT '{}',
  error_code      text CHECK (error_code ~ '^[a-z][a-z0-9_]{1,63}$'),
  error           text,
  last_event      text,
  created_at      timestamptz NOT NULL DEFAULT now(),
  updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON kohral.runs_ledger (session_id, created_at);
CREATE INDEX ON kohral.runs_ledger (updated_at, run_id);

CREATE TABLE kohral.session_messages (
  message_id text PRIMARY KEY, session_id text NOT NULL, run_id uuid NOT NULL,
  role text NOT NULL CHECK (role IN ('user','assistant')), content text NOT NULL,
  created_at timestamptz NOT NULL);
CREATE INDEX ON kohral.session_messages (session_id, created_at);
```

Consistency: acceptance writes the ledger row in the same transaction as
`run.started` (the only synchronous write). Everything after is the
`KohralLedgerProjection`, a bus subscriber with a durable checkpoint:

| Event | Ledger effect |
|---|---|
| `run.understanding_started` | `status='running'`, `last_event` |
| `run.understanding_completed`, `run.plan_proposed`, `task.routed/attempt_started/attempt_succeeded/attempt_failed/retried`, `task.progressed` (throttled), `run.integrated` | `partial_output = partial_output || <rendered line>`, `seq=seq+1`, `usage` merged |
| `run.completed` | `status='completed'`, output reconciled (append unseen suffix), `seq+1` |
| `run.failed` | `status='failed'`, `error_code` from `FailureClass` (`budget_exhausted`, `runtime_restarted`, `runtime_shutdown`, `worker_failed`, `unanswered_question`…), `error` = diagnostic, `seq+1` |
| `run.cancelled` | `status='cancelled'`, `seq+1` |

Updates are idempotent on `(run_id, event position)`; the projection ignores
events for runs already terminal (after a `runtime_restarted` sweep). Because
`seq` only ever increments and `partial_output` only ever appends, a Kohral
worker that polls during a projection lag sees a stale-but-consistent
snapshot, never a regression.

## 3. Questions in Kohral mode

Kohral turns are headless: Kevin **never blocks on a human**. `Run.mode =
Kohral` forces `auto_approve_plans = true` and question policy
`DefaultAfter{0}`: every proposed question is answered by its `default`
(recommended option), recorded as `question.answered{answered_by: "default"}`,
and surfaced in `partial_output` as an **"Assumptions I made"** section so the
operator can correct them in the next turn (the next turn's
`conversation_history` then carries the correction). A question without a
default causes the run to *proceed with the planner's best guess* — it never
fails the run in Kohral mode. `run_approval_response` stays `false` (no
Hermes-style approval round-trip) in v1.

## 4. Agent collaboration (phase 2 — not required for conformance)

Kohral provisions `/run/secrets/kohral-agent-identity` (signed agent identity)
and `KOHRAL_COLLABORATION_URL` (mediated API base). Kevin adds:

- `kevin-kohral::collaboration::Client` for `/api/runtime/v1/collaboration/*`
  (discover peers, delegate, list, get, continue, cancel) authenticated with the
  identity file; request ledger table `kohral.collaboration_requests
  (request_id, session_key, content_hash, status pending|accepted|rejected,
  task_id, created_at)` implementing the reuse rule: an equivalent request whose
  outcome is unknown reuses its `request_id`; once accepted/rejected, identical
  text is new work with a new id; different content under a reused id is a
  `collaboration_idempotency_conflict`.
- An MCP server `kevin mcp collaboration` (stdio) exposing the six required
  tools `list_collaborating_agents`, `delegate_to_agent`, `list_delegated_tasks`,
  `get_delegated_task`, `continue_delegated_task`, `cancel_delegated_task`.
  Workers receive it through their MCP config (claude `--mcp-config`
  `deploy/kohral/mcp.json`; codex/pi/opencode via their MCP config files —
  wiring per worker in 04-workers). The standing system context names the
  capability, as Kohral requires. `delegate_to_agent` returns immediately with
  the durable task id (no synchronous wait).
- Until phase 2 ships, the image does **not** advertise these tools; per Kohral
  docs that is a deployment defect for a production agent, so Kohral rollout of
  Kevin is gated on phase 2 unless the operator accepts a collaboration-less
  runtime.

## 5. Platform briefing, volumes, secrets, overlay

Volume layout inside the `kevin-gateway` container:

| Path | Mount | Content |
|---|---|---|
| `/opt/kevin/config/` | read-only config files from `configFiles()` | `kevin.toml` (operator overlay, §5.2), `AGENTS.md` (mission from anamnesis role), `SOUL.md` (persona + Kohral's `## Kohral` section), `KOHRAL_DOCUMENTATION.md` |
| `/opt/kevin/data/` | persistent volume `data` | `memories/MEMORY.md` (seeded once by entrypoint if absent), worker transcripts, artifacts, workspaces (`/opt/kevin/data/work/<run>`), fastembed model cache |
| `/run/secrets/kohral-runtime-token` | secret | bearer token (`API_SERVER_KEY`) |
| `/run/secrets/kevin-env` | secret env file | provider keys for the CLIs (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, …) sourced by the entrypoint; never logged |
| `/run/secrets/postgres-password` | secret | DB password; entrypoint composes `KEVIN__DATABASE__URL` |
| `/run/secrets/kohral-agent-identity` | secret | collaboration identity (phase 2) |

### 5.1 Briefing injection

At boot `kevin-kohral::briefing` reads `SOUL.md`, `AGENTS.md` and
`KOHRAL_DOCUMENTATION.md`; `kevin-orchestrator` exposes a
`SystemContextProvider` hook and Kohral mode registers one that prepends, in
order: AGENTS.md (mission), SOUL.md (persona + `## Kohral` section, verbatim),
a one-line pointer to `KOHRAL_DOCUMENTATION.md` (never the whole file), then
the per-turn `instructions`. Kohral scans these files for prompt-injection
patterns and replaces a whole file with `[BLOCKED: …]` — Kevin must therefore
treat a `[BLOCKED` file as *missing* and log a warning, not feed it to models.
Entrypoint seeds `/opt/kevin/data/memories/MEMORY.md` with the documentation
pointer only when absent (same reasoning as Hermes: the file belongs to the
agent).

### 5.2 Native configuration overlay

The agent's "native runtime configuration" (Kohral advanced JSON editor) is a
JSON object that Kevin deep-merges as a TOML fragment over its defaults.
Guided fields (§7) write into it. Protected sections that the overlay may not
touch (validated by `kevin config validate --overlay`; Kohral's
`validateConfiguration()` mirrors the list): `server`, `kohral`, `database`,
`sandbox`, `workers.*.bin`, `workers.*.env_passthrough`, `telemetry`. Allowed:
`kevin.auto_approve_plans` (forced true anyway), `budget.*`, `models.*`,
`roles.*`, `routing.*`, `memory.*`, `evaluation.*`, `workspace.*`,
`concurrency.*`. Unknown keys are rejected (deny_unknown_fields) so Kohral
surfaces the error at rollout time.

## 6. Image and stack

`deploy/kohral/Dockerfile` (multi-stage):

1. `FROM rust:1.8x-bookworm AS build` `[inferred — verify current stable]` →
   `cargo build --release -p kevin-cli` with `--locked`; strip.
2. `FROM debian:bookworm-slim` (not distroless: the CLIs need node, bash, git,
   ssh-client, ca-certificates, python3 for codex hooks): install
   `git`, `bash`, `curl`, `ca-certificates`, `openssh-client`, `jq`,
   Node 24 (pinned tarball + sha256 `[inferred — verify]`), then
   `npm install -g @anthropic-ai/claude-code@<pin> @openai/codex@<pin> opencode-ai@<pin> @mariozechner/pi-coding-agent@<pin>` `[inferred — verify package names/pins; codex may prefer its release binary]`; record every pin + digest in `deploy/kohral/upstreams.lock.json` and verify in CI (same discipline as Kohral `runtime/upstreams.lock.json`).
3. Non-root user `kevin` (uid 10000, matching Kohral's volume ownership
   convention `VolumeSpec('data', …, 10000, 10000)`), `HOME=/opt/kevin/data/home`
   so CLI state (`~/.claude`, `~/.codex`, `~/.pi`, `~/.config/opencode`) lands on
   the persistent volume.
4. `COPY --from=build /kevin /usr/local/bin/kevin`; `COPY deploy/kohral/entrypoint.sh`.
5. `EXPOSE 8080`; `HEALTHCHECK CMD curl -fsS http://127.0.0.1:8080/health`.

`entrypoint.sh`: `set -eu`; source `/run/secrets/kevin-env` if present; build
`KEVIN__DATABASE__URL` from `POSTGRES_*` + password file; `kevin db migrate`
(retry until the `postgres` service is ready); seed MEMORY.md; `exec kevin serve
--kohral` (which performs the `runtime_restarted` sweep before binding).

Stack emitted by the strategy (`WorkloadSpec`): services `gateway` (image
`<registry>/kevin@sha256:…`, port 8080, depends on `memory`, cpu 1 / mem 2G,
volumes `data`, config files §5, secrets §5) and `memory` (`pgvector/pgvector:pg17`
digest-pinned `[inferred — verify]`, `POSTGRES_USER=kevin`,
`POSTGRES_DB=kevin`, `POSTGRES_PASSWORD_FILE`, volume `memory-data`, healthcheck
`pg_isready`). One isolated stack per agent (Podman Compose project / k8s
namespace / Swarm stack), exactly like Hermes.

Sandbox tier in this image is `container`: the stack *is* the isolation
boundary (restricted namespace/network policy, no engine socket, egress proxy),
so `workers.claude.permission_mode = "bypassPermissions"` and `codex -s
workspace-write` (not `danger-full-access`) are allowed for unattended runs.
Limits of the argument: the workers can still read everything in the data
volume (including the CLI auth state) and reach whatever egress Kohral permits;
secrets are therefore limited to what the agent legitimately needs, and Kevin
keeps its own `env_passthrough` allow-list per worker.

## 7. Kohral-side deliverable: `KevinRuntimeStrategy` (separate PR in Kohral)

Steps from Kohral `docs/07` "Adding a runtime later": implement the strategy,
register it in `RuntimeRegistry` (compiler pass), add a client picker entry,
no domain changes.

```php
final class KevinRuntimeStrategy implements AgentRuntimeStrategy
{
    public function type(): string { return 'kevin'; }
    public function label(): string { return 'Kevin'; }
    public function supportedModes(): array { return ['flex', 'live_standard', 'live_plus']; }
    public function image(string $version): string { /* digest from image-lock */ }
    public function validateConfiguration(array $c): void { RuntimeConfiguration::assertSafe($c, ['server','kohral','database','sandbox','telemetry']); }
    public function configurationGuide(): array { return [
        'documentationUrl' => 'https://github.com/Ligerian-labs/kevin/blob/main/plan/03-config-schema.md',
        'protectedSections' => ['server','kohral','database','sandbox','telemetry'],
        'example' => ['budget' => ['default_run_usd' => 10], 'roles' => ['planner' => 'opus5-claude']],
        'fields' => [
            ['id' => 'primary-model', 'path' => ['roles','planner'], 'label' => 'Planner model alias', 'input' => 'text', 'defaultValue' => 'opus5-claude', 'managed' => true, …],
            ['id' => 'default-model', 'path' => ['roles','default'], 'label' => 'Default worker model alias', 'input' => 'text', 'defaultValue' => 'sonnet5-claude', …],
            ['id' => 'effort', 'path' => ['roles','effort','planner'], 'label' => 'Planner effort', 'input' => 'select', 'options' => ['low','medium','high','xhigh','max'], 'defaultValue' => 'xhigh', …],
            ['id' => 'run-budget', 'path' => ['budget','default_run_usd'], 'label' => 'Budget per turn (USD)', 'input' => 'integer', 'defaultValue' => 10, 'min' => 1, …],
            ['id' => 'parallel', 'path' => ['budget','max_parallel_tasks'], 'label' => 'Parallel tasks', 'input' => 'integer', 'defaultValue' => 4, 'min' => 1, …],
        ]]; }
    public function configFiles(Anamnesis $a, string $agentName = '', string $user = ''): array {
        // '/opt/kevin/config/kevin.toml' (overlay → TOML), '/opt/kevin/config/AGENTS.md' (mission),
        // '/opt/kevin/config/SOUL.md' ($briefing->append(...)), '/opt/kevin/config/KOHRAL_DOCUMENTATION.md'
    }
    public function secretBindings(): array { return ['KEVIN_RUNTIME_TOKEN' => 'API_SERVER_KEY', 'KEVIN_POSTGRES_PASSWORD' => 'POSTGRES_PASSWORD']; }
    public function credentialRequirements(): array { /* token, pg password, ANTHROPIC_API_KEY | CLAUDE_CODE_OAUTH_TOKEN | OPENAI_API_KEY (oneOfGroup model-provider) */ }
    public function buildSpec(...): WorkloadSpec { /* gateway + memory services as in §6 */ }
    public function supportedChannels(): array { return []; }          // Kevin has no channels in v1
    public function channelSecretKeys(string $c): array { return []; }
    public function channelDescriptor(string $c): array { throw new \DomainException('Kevin has no channels.'); }
    public function configureChannel(WorkloadSpec $s, ChannelConfiguration $c): WorkloadSpec { throw new \DomainException('Kevin has no channels.'); }
    public function channelStatus(...): array { return ['status' => 'unsupported', 'detail' => '']; }
    public function health(string $endpoint): array { return $this->telemetry->health(rtrim($endpoint,'/').'/health'); }
    public function sessions(...): array { /* GET /api/sessions (+ /messages) */ }
    public function session(...): ?array { /* GET /api/sessions/{id}/messages */ }
    public function prepareConversationModel(...): void {}
    public function chatRequest(...): RuntimeChatRequest { throw new \DomainException('Kevin only supports the durable run contract.'); }
    public function conversationCompatibility(...): RuntimeConversationCompatibility { /* same checks as Hermes */ }
    public function submitTurn(...)/turnStatus(...)/interruptTurn(...)/beginDrain/drainState/cancelDrain { /* identical to HermesRuntimeStrategy */ }
    public function metrics(...): array { /* GET /health/detailed → numeric */ }
}
```

Because `submitTurn/turnStatus/interruptTurn/drain*` are byte-identical to the
Hermes implementation, the recommended Kohral refactor is to extract a
`HermesStyleDurableRunClient` trait shared by both strategies (Kohral-side
decision; flag `[inferred — verify]` with Kohral maintainers).

## 8. Conformance

`kevin kohral conformance [--image <ref>] [--compose]` wraps Kohral's
`runtime/conformance/contract.py --runtime hermes`:

1. Start the stack (`deploy/kohral/compose.conformance.yaml`: gateway with the
   conformance profile, pgvector) and wait for `/health`.
2. `contract.py basic --runtime hermes --base-url http://127.0.0.1:8080 --token $T`
   → capabilities, model catalog (+401 on wrong token), submit/retry/409, terminal
   `completed` with output `kohral-ok`.
3. `contract.py accept-crash … --run-id-file run.id` → submits `[[KOHRAL_HOLD]]`,
   waits non-terminal; then `docker kill` the gateway; restart.
4. `contract.py verify-crash … --run-id-file run.id` → status `failed`,
   `error_code == runtime_restarted`.

CI job `kohral-conformance` (GitHub Actions, `linux/amd64`; `arm64` under QEMU
with the longer timeouts Kohral documents) runs on every change to
`crates/kevin-kohral/**`, `deploy/kohral/**`, and on release tags; release
images are built with provenance + SBOM and signed with Cosign, referenced by
digest (Kohral consumes digests only).

## Deliverables checklist (maps to workstreams)

- [ ] `kevin-kohral` router mounted by `kevin serve --kohral` with every endpoint in §1.1 and the exact capabilities/catalog payloads.
- [ ] `kohral.runs_ledger`, `kohral.session_messages` migrations + `KohralLedgerProjection` + `runtime_restarted` boot sweep.
- [ ] Kohral-mode run behaviour: defaults for questions, assumptions section, auto-approve.
- [ ] Fake-worker conformance hooks behind the conformance profile.
- [ ] `deploy/kohral/{Dockerfile,entrypoint.sh,compose.conformance.yaml,conformance.toml,upstreams.lock.json}`.
- [ ] `kevin kohral conformance` + CI job.
- [ ] Phase 2: collaboration client, request ledger, `kevin mcp collaboration`.
- [ ] Kohral PR: `KevinRuntimeStrategy`, registry entry, client picker, image-lock entry.

---
Summary: Endpoints — `/health`, `/v1/health`, `/health/detailed`, `/v1/capabilities`, `/v1/kohral/models`, `POST /v1/runs`, `GET /v1/runs/{id}`, `POST /v1/runs/{id}/stop`, `/api/sessions[/{id}[/messages]]`, `/v1/maintenance/drain`, `/v1/attachments/...`. Tables — `kohral.runs_ledger`, `kohral.session_messages`, (phase 2) `kohral.collaboration_requests`. Behaviours — Idempotency-Key acceptance in one tx, canonical request hash, append-only `partial_output` + monotonic `seq`, `runtime_restarted` sweep at boot, no replay, drain, fake-worker conformance hooks, Kohral-mode default answers, `KevinRuntimeStrategy` on the Kohral side.
