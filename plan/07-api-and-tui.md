# 07 — API, CLI and TUI

Crates: `kevin-api` (axum server + typed client), `kevin-cli` (the `kevin`
binary), `kevin-tui` (ratatui client). The TUI and the CLI never touch the
store; they are API clients. Everything the TUI can do, the API can do, and
everything the API can do maps to a command or a projection query from
[02-domain-model](./02-domain-model.md).

## 1. HTTP API (`kevin-api`)

- Framework: `axum` 0.8, `tower-http` (trace, timeout, request-id, cors,
  limit), `utoipa` + `utoipa-axum` for OpenAPI (`GET /api/v1/openapi.json`,
  Swagger UI at `/api/v1/docs` when `server.docs = true`, default true on the
  laptop profile).
- Bind: `server.bind` (default `127.0.0.1:7777`). Binding a non-loopback
  address without TLS termination in front logs a `warn` at startup.
- Versioning: everything under `/api/v1`. Health and metrics are unversioned.
- Request ids: `x-request-id` honoured or generated (uuid v7); echoed back and
  used as `causation_id` for commands.

### Authentication

- `Authorization: Bearer <token>`; token read from `server.auth_token_file`
  (0600, created by `kevin config init` with 32 random bytes, base64url).
  Compare with `subtle::ConstantTimeEq`. Missing/invalid → `401
  {code:"unauthenticated"}`.
- Exempt from auth: `/healthz`, `/readyz`, `/metrics` (bind-level protection),
  `/api/v1/openapi.json`.
- Kohral mode uses its own token (`kohral.token_file`) on the Kohral routes
  only (see [08](./08-kohral-runtime.md)); the two surfaces never share a
  token.
- Token rotation: `kevin config rotate-token` writes a new token; the server
  reloads the token file on SIGHUP and accepts old+new for
  `server.token_grace = "5m"`.

### Conventions

- JSON everywhere; timestamps RFC 3339 UTC; ids are uuid strings; money is a
  decimal string (`"0.0421"`), never a float.
- Error envelope (every non-2xx):

```json
{ "code": "run_not_found", "message": "run 0191… does not exist", "details": { "run_id": "0191…" }, "request_id": "0191…" }
```

  Codes are stable and language-neutral (clients translate). Full list:

  | HTTP | `code` |
  |---|---|
  | 400 | `invalid_request`, `invalid_goal`, `invalid_answer`, `invalid_cursor`, `payload_too_large` |
  | 401 | `unauthenticated` |
  | 403 | `forbidden` (loopback-only endpoint hit remotely) |
  | 404 | `run_not_found`, `task_not_found`, `question_not_found`, `proposal_not_found`, `artifact_not_found` |
  | 409 | `idempotency_conflict`, `run_not_in_state`, `task_not_in_state`, `question_already_answered`, `version_conflict` |
  | 422 | `plan_invalid`, `budget_invalid`, `unknown_model_alias`, `worker_disabled` |
  | 429 | `rate_limited` |
  | 503 | `draining`, `db_unavailable`, `runtime_unavailable` |
  | 500 | `internal` |

- Idempotency: `POST /api/v1/runs` accepts `Idempotency-Key` (≤128 chars,
  `[A-Za-z0-9._:-]`). It becomes the `command_id`; replay with identical body
  → `200` with the original run; different body → `409 idempotency_conflict`.
  Same mechanism for `POST …/answer` and `…/approve-plan`.
- Pagination: `?cursor=<opaque>&limit=50` (max 200); response `{ items, next_cursor }`.
  Cursors are base64url of `(sort_key, id)`.
- Limits: request body 1 MiB (goal text ≤ 64 KiB, attachments by reference
  only); SSE keep-alive every `server.sse_keepalive`; per-token rate limit 60
  req/s burst 120 (tower `governor`), SSE connections ≤ 64 per token.
- Every handler has `server.request_timeout` except SSE streams.

### Endpoints

| Method | Path | Request → Response | Command / query |
|---|---|---|---|
| POST | `/api/v1/runs` | `CreateRunRequest` → `201 RunDto` | `StartRun` |
| GET | `/api/v1/runs` | `?status=&cursor=&limit=` → `Page<RunSummaryDto>` | `orch.run_overview` |
| GET | `/api/v1/runs/{run_id}` | → `RunDto` (incl. understanding, plan, open questions, usage) | `orch.run_overview` |
| POST | `/api/v1/runs/{run_id}/cancel` | `{reason?}` → `202 RunDto` | `CancelRun` |
| POST | `/api/v1/runs/{run_id}/plan/approve` | `{}` → `202 RunDto` | `ApprovePlan` |
| POST | `/api/v1/runs/{run_id}/plan/reject` | `{feedback}` → `202 RunDto` | `RejectPlan` |
| POST | `/api/v1/runs/{run_id}/evaluate` | `{}` → `202` | `Evaluate` (re-run judge) |
| GET | `/api/v1/runs/{run_id}/tasks` | → `Vec<TaskDto>` | `orch.task_board` |
| GET | `/api/v1/tasks/{task_id}` | → `TaskDto` (attempts, route, usage, artifacts) | `orch.task_board` |
| POST | `/api/v1/tasks/{task_id}/retry` | `{exclude_route?: bool}` → `202 TaskDto` | `RetryTask` |
| POST | `/api/v1/tasks/{task_id}/cancel` | → `202 TaskDto` | `CancelTask` |
| GET | `/api/v1/tasks/{task_id}/log` | `?attempt=&after_seq=&limit=` → `Page<TaskLogLineDto>` | `orch.task_log` |
| GET | `/api/v1/tasks/{task_id}/artifacts` | → `Vec<ArtifactDto>` | `orch.artifacts` |
| GET | `/api/v1/artifacts/{artifact_id}` | → bytes (`content-type` from kind) | blob store |
| GET | `/api/v1/questions` | `?status=open&run_id=` → `Page<QuestionDto>` | `orch.question_inbox` |
| GET | `/api/v1/questions/{question_id}` | → `QuestionDto` | |
| POST | `/api/v1/questions/{question_id}/answer` | `AnswerRequest` → `200 QuestionDto` | `AnswerQuestion` |
| GET | `/api/v1/runs/{run_id}/events` | SSE (`Last-Event-ID`) | bus + `core.events` catch-up |
| GET | `/api/v1/events` | SSE firehose `?types=run.*,task.*` | bus + store |
| GET | `/api/v1/cost` | `?since=&group_by=run\|model\|kind` → `CostReportDto` | `orch.cost_ledger` |
| GET | `/api/v1/routes` | `?kind=` → `Vec<RouteScoreDto>` | `routing.route_leaderboard` |
| GET | `/api/v1/memory/search` | `?q=&kinds=&top_k=` → `Vec<MemoryItemDto>` | kevin-memory search |
| GET | `/api/v1/lessons` | `?cursor=` → `Page<MemoryItemDto>` (kind=lesson) | `memory.lessons_view` |
| DELETE | `/api/v1/memory/{item_id}` | → `204` | `ForgetMemoryItem` |
| GET | `/api/v1/proposals` | `?status=proposed` → `Page<ProposalDto>` | `eval.proposals_inbox` |
| POST | `/api/v1/proposals/{id}/accept` / `/reject` | `{note?}` → `200 ProposalDto` | `AcceptProposal` / `RejectProposal` |
| GET | `/api/v1/workers` | → `Vec<WorkerDoctorDto>` | `Worker::doctor()` |
| GET | `/api/v1/config` | → redacted effective config + sources | kevin-config |
| GET | `/healthz` | `200 {status:"ok"}` | process alive |
| GET | `/readyz` | `200/503 {db, draining, workers_ok}` | db ping ≤ 1s, not draining |
| POST/GET/DELETE | `/api/v1/maintenance/drain` | → `DrainStatusDto {draining, running_runs, running_attempts}` | orchestrator admission gate |
| GET | `/metrics` | Prometheus text | only when `telemetry.metrics_bind` is set (served on that bind, not here) |

### DTOs (module `kevin_api::dto`, `serde` + `utoipa::ToSchema`)

```rust
pub struct CreateRunRequest { pub goal: String, pub cwd: Option<PathBuf>, pub attachments: Vec<AttachmentRef>,
    pub mode: Option<RunModeDto /* interactive|headless */>, pub budget: Option<BudgetDto>, pub tags: Vec<String> }
pub struct RunDto { pub id: RunId, pub status: RunStatusDto, pub goal: GoalDto, pub mode: RunModeDto, pub budget: BudgetDto,
    pub usage: UsageDto, pub understanding: Option<UnderstandingDto>, pub plan: Option<PlanDto>, pub open_questions: Vec<QuestionId>,
    pub tasks: Vec<TaskSummaryDto>, pub evaluation: Option<EvaluationSummaryDto>, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>, pub version: u64 }
pub struct RunSummaryDto { pub id, pub status, pub goal_excerpt: String, pub usage: UsageDto, pub task_counts: TaskCountsDto, pub created_at, pub updated_at }
pub struct TaskDto { pub id: TaskId, pub run_id: RunId, pub kind: TaskKind, pub title: String, pub status: TaskStatusDto, pub route: Option<RouteDto>,
    pub attempts: Vec<AttemptDto>, pub depends_on: Vec<TaskId>, pub usage: UsageDto, pub artifacts: Vec<ArtifactDto>, pub acceptance_criteria: Vec<String> }
pub struct AttemptDto { pub id: AttemptId, pub no: u8, pub route: RouteDto, pub status: String, pub workspace: Option<WorkspaceDto>,
    pub worker_session_id: Option<String>, pub started_at, pub ended_at: Option<_>, pub usage: UsageDto, pub failure: Option<FailureDto> }
pub struct TaskLogLineDto { pub seq: u64, pub attempt: u8, pub at: DateTime<Utc>, pub kind: String /* assistant|tool_call|tool_result|usage|system */, pub payload: serde_json::Value }
pub struct QuestionDto { pub id: QuestionId, pub run_id: RunId, pub task_id: Option<TaskId>, pub text: String, pub options: Vec<QuestionOptionDto>,
    pub multi_select: bool, pub default: Option<AnswerDto>, pub policy: QuestionPolicyDto, pub status: String, pub answer: Option<AnswerDto>, pub asked_at }
pub struct AnswerRequest { pub selected: Vec<String>, pub free_text: Option<String> }
pub struct EventDto { pub position: u64, pub event_id: EventId, pub event_type: String, pub occurred_at, pub aggregate_type: String,
    pub aggregate_id: Uuid, pub aggregate_version: u64, pub correlation_id: Uuid, pub payload: serde_json::Value }
pub struct CostReportDto { pub total_usd: Option<Decimal>, pub total_tokens: u64, pub rows: Vec<CostRowDto { key: String, usd: Option<Decimal>, input_tokens, output_tokens, attempts: u32 }> }
pub struct RouteScoreDto { pub kind: TaskKind, pub alias: ModelAlias, pub attempts: u32, pub successes: u32, pub mean_quality: Option<f32>, pub mean_cost_usd: Option<Decimal>, pub mean_wall_ms: Option<u64>, pub sampled_score: Option<f32> }
pub struct MemoryItemDto { pub id, pub kind: String, pub content: String, pub tags: Vec<String>, pub importance: f32, pub similarity: Option<f32>, pub source: serde_json::Value, pub created_at }
pub struct ProposalDto { pub id, pub evaluation_id, pub kind: String /* prompt|config|routing */, pub body: String, pub status: String, pub created_at }
pub struct WorkerDoctorDto { pub kind: WorkerKind, pub enabled: bool, pub binary: Option<PathBuf>, pub version: Option<String>, pub auth_ready: Option<bool>, pub problems: Vec<String> }
pub struct DrainStatusDto { pub draining: bool, pub running_runs: u32, pub running_attempts: u32 }
pub struct Page<T> { pub items: Vec<T>, pub next_cursor: Option<String> }
```

Example — create a run:

```http
POST /api/v1/runs
Authorization: Bearer …
Idempotency-Key: cli-0191f3a0-…
{ "goal": "Add a /healthz endpoint to the axum app and tests", "cwd": "/home/v/workspace/app", "mode": "interactive", "budget": { "max_usd": "5.00" } }

201 { "id": "0191f3a1-…", "status": "received", "goal": {…}, "mode": "interactive", "budget": {"max_usd":"5.00","max_attempts":2,"max_parallel":4}, "usage": {"input_tokens":0,"output_tokens":0,"cost_usd":"0"}, "open_questions": [], "tasks": [], "version": 1, … }
```

### Event streams (SSE)

- `GET /api/v1/runs/{run_id}/events` and `GET /api/v1/events` emit
  `text/event-stream`; each message: `id: <global position>`,
  `event: <event_type>`, `data: <EventDto JSON>`. `:keepalive` comment every
  `server.sse_keepalive`.
- On connect with `Last-Event-ID: <position>` the handler first replays
  `core.events` where `position > last` (filtered by `correlation_id = run_id`
  or by `?types=`), then switches to the live bus subscription; a sequence
  guard drops duplicates across the seam. Without `Last-Event-ID`, `?from=0`
  replays all; default is live-only plus one synthetic `snapshot` event
  carrying the current `RunDto`.
- Bus lag (`tokio::sync::broadcast::error::RecvError::Lagged`) → the stream
  emits `event: resync` and the client must refetch the snapshot and
  reconnect with the last position it has.
- Task log lines are **not** on the SSE stream (volume); clients poll
  `/tasks/{id}/log?after_seq=` or subscribe to `GET
  /api/v1/tasks/{task_id}/log/stream` (SSE of `TaskLogLineDto`, same
  `Last-Event-ID` = seq convention).

### Module layout

```text
crates/kevin-api/src/
  lib.rs          // pub fn router(state: AppState) -> axum::Router ; pub mod client; pub mod dto
  state.rs        // AppState { services: Arc<Services>, bus, store, config, drain: DrainGate, token: TokenVerifier }
  auth.rs         // Bearer extractor, constant-time compare, SIGHUP reload
  error.rs        // ApiError { code, status, message, details } + IntoResponse ; From<DomainError>, From<StoreError>
  routes/{runs,tasks,questions,events,cost,routes,memory,proposals,workers,config,maintenance,health}.rs
  sse.rs          // catch-up + live merge, keepalive, Lagged → resync
  dto.rs          // DTOs + From<domain/projection> impls
  openapi.rs      // utoipa ApiDoc
  client/{mod,runs,events,…}.rs   // typed client (feature "client", no axum dep)
```

## 2. Typed client (`kevin_api::client`)

```rust
pub struct KevinClient { base: Url, token: SecretString, http: reqwest::Client }
impl KevinClient {
    pub fn new(base: Url, token: SecretString) -> Self;
    pub async fn create_run(&self, req: CreateRunRequest, idem: Option<&str>) -> Result<RunDto, ClientError>;
    pub async fn get_run(&self, id: RunId) -> Result<RunDto, ClientError>;
    pub async fn list_runs(&self, q: ListRunsQuery) -> Result<Page<RunSummaryDto>, ClientError>;
    pub async fn cancel_run / approve_plan / reject_plan / retry_task / cancel_task / answer_question / accept_proposal …
    pub fn run_events(&self, id: RunId, from: Option<u64>) -> impl Stream<Item = Result<EventDto, ClientError>>; // auto-reconnect
    pub fn task_log_stream(&self, id: TaskId, after_seq: Option<u64>) -> impl Stream<Item = Result<TaskLogLineDto, ClientError>>;
    pub async fn questions(&self, q: QuestionsQuery) -> …; pub async fn routes(&self) -> …; pub async fn cost(&self, q) -> …;
    pub async fn workers(&self) -> …; pub async fn drain(&self, on: bool) -> …; pub async fn ready(&self) -> Result<ReadyDto, ClientError>;
}
pub enum ClientError { Api { status: u16, code: String, message: String, details: Option<Value> }, Transport(reqwest::Error), Stream(String), Resync }
```

- SSE via `reqwest-eventsource`; reconnect with exponential backoff (250 ms →
  10 s, jitter), resending `Last-Event-ID`; `resync` events surface as
  `ClientError::Resync` so the consumer refetches a snapshot.
- Feature-gated (`kevin-api/client`) so `kevin-tui` compiles without axum.

## 3. CLI (`kevin-cli`, clap derive)

```text
kevin [--config <file>] [--set <k=v>]... [--server <url>] [--token-file <path>] [--json] [-v|-q]
  run <goal> [--cwd <dir>] [--headless] [--budget-usd <dec>] [--budget-wall <dur>] [--attach <file>]... [--no-tui] [--wait] [--tag <t>]...
  serve [--kohral] [--bind <addr>]
  tui [--run <run-id>]
  runs   ls [--status <s>] [--limit N] | show <run-id> | cancel <run-id> [--reason] | events <run-id> [--from N] | watch <run-id>
  tasks  ls <run-id> | show <task-id> | log <task-id> [--follow] [--attempt N] | retry <task-id> | cancel <task-id>
  questions ls [--run <id>]
  answer <question-id> [<option>...] [--text <free text>] [--default]
  approve <run-id> | reject <run-id> --feedback <text>
  db     init [--create-role] | migrate | status | reset --yes | prune | rebuild-projection <name|--all>
  config init [--force] | show [--sources] | validate | rotate-token
  workers doctor
  routes [--kind <kind>] | explain --kind <kind> [--complexity c] | reset --kind <kind> --alias <alias>
  lessons [--limit N] [--repo]
  memory search <query> [--kinds a,b] [--top-k N] | add --kind fact|preference <text> [--tag t] [--global] | forget <item-id>|--run <run>|--repo <scope>|--all-before <date> | reindex [--model m] | doctor | export --json | import <file>
  eval   rerun <run-id>
  proposals ls | show <id> | accept <id> [--note] | reject <id> [--note]
  cost [--since <dur>] [--group-by run|model|kind] [--run <run-id>]
  kohral conformance [--base-url <url>] [--token <t>] [--phase basic|accept-crash|verify-crash]
  completions <shell>
```

Behaviour:

- Global `--json` makes every command print machine-readable JSON (one object,
  or JSON lines for streams); otherwise human tables (`comfy-table`) and
  colours when stdout is a TTY.
- Server resolution: `--server` > `KEVIN__CLIENT__SERVER_URL` > `client.server_url`.
  When empty, **embedded mode**: `kevin run`/`kevin tui`/`kevin runs …` start
  the runtime in-process (config load → db migrate if `auto_migrate` → store,
  bus, workers, orchestrator → API bound on `127.0.0.1:0`) and point the
  internal `KevinClient` at it. The ephemeral address and token are written to
  `$data_dir/run/embedded.json` (0600) so a second `kevin tui` in another
  terminal can attach while the first process lives; the file is removed on
  exit.
- `kevin run`:
  1. resolve cwd (must be inside a git/jj repo unless `--cwd` is given with
     `--allow-plain-dir`), read stdin as extra goal context if piped;
  2. `POST /runs` with `Idempotency-Key = cli-<uuid>`;
  3. if TTY and not `--no-tui` → open the TUI focused on this run (`kevin tui
     --run`), else stream pretty lines (`[12:00:01] task.attempt_started
     implement:…`) or JSON lines with `--json`; questions in non-TUI mode are
     answered inline on the terminal when stdin is a TTY, otherwise the run
     waits in `awaiting_answers` and the CLI prints the `kevin answer` hint;
  4. `--headless` forces `mode = headless` (auto-approve plan, defaults for
     questions);
  5. exit codes: `0` completed, `1` failed, `2` cancelled, `3` invalid
     arguments/config, `4` server unreachable/unauthenticated, `5` budget
     exhausted, `130` Ctrl-C (which **cancels** the run in embedded mode and
     **detaches** in server mode unless `--cancel-on-detach`).
  `--wait` with `--no-tui` blocks until terminal state even when detached.
- `kevin serve`: foreground, structured logs to stdout, SIGTERM/SIGINT →
  graceful shutdown per [01](./01-architecture.md); `--kohral` sets
  `kevin.profile = kohral`.
- `kevin db init`: creates role/database/extension `vector` using
  `database.url` (or `--admin-url`), then migrates. `reset` requires `--yes`
  and refuses when `kevin.profile != laptop`.
- `kevin workers doctor`: table of `WorkerDoctorDto`; exit 1 if any enabled
  worker is unhealthy.
- `kevin config init`: writes the commented default file from
  [03](./03-config-schema.md) plus a fresh token; never overwrites without
  `--force`.

## 4. TUI (`kevin-tui`)

- Stack: `ratatui` + `crossterm`, `tokio` for the client tasks, `tui-input`
  for text fields. No direct store access; one `KevinClient`.
- Architecture: Elm-style. `Model` (pure state) + `Msg` (key events,
  `ApiEvent(EventDto)`, snapshots, tick, client errors) + `update(&mut Model,
  Msg) -> Vec<Cmd>` (pure reducer) + `view(&Model, &mut Frame)`. `Cmd`s
  (HTTP calls, subscribe, copy to clipboard) are executed by a runtime task
  and fed back as `Msg`s. This keeps the reducer unit-testable without a
  terminal.
- Data flow: on start, fetch `list_runs` snapshot + subscribe to
  `/api/v1/events` (firehose filtered `run.*,task.*,question.*`); selecting a
  run fetches `RunDto` + tasks and opens the run stream with
  `Last-Event-ID`; a `Resync`/gap (non-contiguous aggregate_version for the
  same aggregate) triggers a snapshot refetch. Task transcript pane subscribes
  to `task_log_stream` for the focused task only.
- Bounded buffers: 5 000 log lines per focused task (ring buffer), 500 events
  per run in the timeline; older lines are fetched on scroll-up via
  `/tasks/{id}/log?after_seq=`.

### Screens

| Screen | Content | Key actions |
|---|---|---|
| **Runs** (home) | table: id (short), status badge, goal excerpt, tasks done/total, cost, age; filter by status; footer shows server URL, draining flag, open-question count | `Enter` open, `n` new run (prompt modal), `c` cancel, `/` filter, `r` refresh |
| **Run detail** | left: phase timeline (received → … → completed with timestamps); centre: task board grouped by status (pending/routed/running/awaiting_input/succeeded/failed/skipped) with route alias + attempt no + elapsed; right: focused task transcript (follow mode) ; bottom: cost footer (`$0.42 / $5.00`, tokens) + budget gauge + wall-clock gauge | `Tab` cycle panes, `j/k` move, `Enter` focus task, `f` toggle follow, `a` approve plan (when awaiting), `x` reject plan (opens feedback), `R` retry task, `C` cancel run/task, `q` question inbox, `o` open artifact path, `y` yank id |
| **Question inbox** (modal, also reachable globally with `?`) | list of open questions across runs; detail: text, options with descriptions, recommended marker, default + deadline countdown | `j/k` select, `Space` toggle (multi-select) / `Enter` choose, `t` free text, `Enter` submit, `Esc` back |
| **Plan approval** (modal when run in `awaiting_plan_approval`) | task DAG rendered as an indented tree (topological order, deps shown as `← dep titles`), each row: kind, title, suggested tier, parallel-safe flag, acceptance criteria count; rationale panel | `a` approve, `x` reject with feedback, `Enter` expand task, `Esc` later |
| **Routes** | leaderboard per kind: alias, attempts, success %, mean quality, mean cost, mean latency, sampled score | `k` change kind, `s` sort |
| **Lessons & proposals** | two tabs: lessons (content, tags, importance, source run) ; proposals (kind, body, status) | `A` accept, `X` reject, `d` forget lesson, `/` search (calls memory search) |
| **Workers** | doctor table | `r` refresh |

Global keys: `1..6` switch screens, `?` inbox, `:` command palette (same verbs
as the CLI, e.g. `:cancel`, `:answer`), `g`/`G` top/bottom, `Ctrl-c`/`Q` quit
(never cancels runs in server mode; in embedded mode asks "cancel running
run?"), `L` toggle log level pane (client errors, reconnects).

### Rendering rules

- Status colours: running = yellow, succeeded/completed = green, failed = red,
  awaiting_* = magenta, cancelled/skipped = dim. Monochrome fallback when
  `NO_COLOR` is set.
- Minimum terminal 80×24: below that, panes collapse to a single column with
  tabs.
- Every modal shows its keybindings in the footer; no action is mouse-only
  (mouse scroll/click supported but optional).

### Tests

- Reducer tests: feed `Msg` sequences, assert `Model` (no terminal).
- Snapshot tests with `ratatui::backend::TestBackend` for each screen at
  80×24 and 120×40 using `insta`.
- An integration test boots the embedded runtime with the `fake` worker, runs
  a scripted scenario, and drives the TUI reducer from real API events.

## 5. Deployment notes

systemd unit (VPS profile):

```ini
[Unit]
Description=Kevin agent runtime
After=network-online.target postgresql.service
[Service]
User=kevin
Environment=KEVIN__KEVIN__PROFILE=server
EnvironmentFile=-/etc/kevin/kevin.env      # KEVIN__DATABASE__URL, etc.
ExecStart=/usr/local/bin/kevin serve
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure
TimeoutStopSec=45                          # ≥ kevin.shutdown_grace_period
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/kevin
[Install]
WantedBy=multi-user.target
```

- Put TLS termination (Caddy/nginx) in front and keep `server.bind` on
  loopback or a private interface; SSE requires `proxy_buffering off` /
  `flush_interval -1`.
- Clients on other machines: `kevin tui --server https://kevin.example --token-file ~/.config/kevin/vps.token`.
- `kevin config rotate-token` + `systemctl reload kevin` rotates without
  downtime (grace window).
- Readiness for orchestrators: `/readyz` returns 503 while draining so a
  load balancer stops sending new runs; `/healthz` stays 200.

## Summary

- Prefix `/api/v1`; unversioned `/healthz`, `/readyz`, `/metrics`; SSE at
  `/runs/{id}/events`, `/events`, `/tasks/{id}/log/stream` with
  `Last-Event-ID` = global position / log seq.
- Modules: `kevin_api::{router, state, auth, error, routes::*, sse, dto,
  openapi, client}`; DTOs `RunDto`, `TaskDto`, `AttemptDto`, `QuestionDto`,
  `EventDto`, `CostReportDto`, `RouteScoreDto`, `MemoryItemDto`,
  `ProposalDto`, `WorkerDoctorDto`, `DrainStatusDto`, `Page<T>`; errors
  `ApiError{code,…}` with the stable code list above.
- Client: `kevin_api::client::KevinClient` (reqwest + reqwest-eventsource,
  reconnect with `Last-Event-ID`, `ClientError::Resync`).
- CLI: `kevin run|serve|tui|runs|tasks|questions|answer|approve|reject|db|config|workers|routes|lessons|memory|eval|proposals|cost|kohral|completions`; embedded runtime when `client.server_url` is empty; exit codes 0/1/2/3/4/5/130.
- TUI: Elm-style reducer over API events; screens Runs, Run detail, Question inbox, Plan approval, Routes, Lessons & proposals, Workers.
