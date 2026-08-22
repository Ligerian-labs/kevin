# 04 — Workers (`kevin-worker`)

A **worker** drives one external coding-agent CLI (`claude`, `codex`, `pi`,
`opencode`) or the in-process `fake` worker to execute one **task attempt**.
Workers know nothing about runs, routing or evaluation; they receive a
`TaskAttemptRequest`, spawn a process in the attempt's workspace, normalise its
output into `WorkerEvent`s and finish with a `WorkerOutcome`. The orchestrator
folds those into `task.*` domain events (see [02](./02-domain-model.md)).

Kevin never calls provider HTTP APIs in v1; every model role goes through a
worker (principle 2 in [00](./00-vision.md)).

## Core types

```rust
// crates/kevin-worker/src/lib.rs
pub struct TaskAttemptRequest {
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    pub run_id: RunId,                          // correlation id for logs/transcripts
    pub kind: TaskKind,
    pub spec: TaskSpec,                         // title, instructions, inputs, acceptance_criteria, output_schema
    pub route: Route,                           // worker, model alias, effort
    pub model: ModelEntry,                      // resolved [models.<alias>] entry (model id, provider, extras)
    pub workspace: Workspace,                   // cwd for the process
    pub context: AttemptContext,                // system_prompt_append, memory: Option<String> (rendered <kevin-memory> block, 06 §1.6), prior_session: Option<WorkerSessionId>
    pub env: EnvAllowlist,                      // computed from workers.<kind>.env_passthrough + sandbox.env_allowlist_extra
    pub budget: AttemptBudget,                  // timeout: Duration, max_tokens: Option<u64>, max_turns: Option<u32>
    pub cancel: CancellationToken,              // child of the task token
}

pub struct WorkerSessionId(pub String);         // worker-native session id, used for follow-ups/resume

pub enum WorkerEvent {
    Started      { session_id: Option<WorkerSessionId>, pid: Option<u32> },
    AssistantText{ delta: String },
    Thinking     { delta: String },             // only when the CLI exposes it
    ToolCall     { name: String, input_summary: String },
    ToolResult   { name: String, ok: bool, output_summary: String },
    Usage        { delta: Usage },              // additive; cost_usd only if reported by the CLI
    InputRequested { question: String, options: Vec<String> },   // folded into task.input_requested
    Final        { text: String, structured: Option<serde_json::Value>, usage: Usage },
    Failed       { class: FailureClass, message: String, usage: Usage },
}

pub struct WorkerHandle {
    pub events: tokio::sync::mpsc::Receiver<WorkerEvent>,   // bounded (cap 256) → back-pressure on the child
    pub session_id: watch::Receiver<Option<WorkerSessionId>>,
    cancel: CancellationToken,
    join: JoinHandle<()>,
}
impl WorkerHandle {
    pub fn cancel(&self);                                   // SIGTERM → grace → SIGKILL
    pub async fn wait(self) -> WorkerOutcome;               // drains events; terminal state
}

pub enum WorkerOutcome {
    Succeeded { text: String, structured: Option<serde_json::Value>, usage: Usage, session_id: Option<WorkerSessionId>, transcript: ArtifactRef },
    Failed    { class: FailureClass, message: String, usage: Usage, transcript: Option<ArtifactRef> },
}

pub struct Doctor { pub kind: WorkerKind, pub binary: Option<PathBuf>, pub version: Option<String>,
                    pub auth_ready: AuthStatus /* Ready | Missing(hint) | Unknown */, pub notes: Vec<String> }

#[async_trait]
pub trait Worker: Send + Sync {
    fn kind(&self) -> WorkerKind;
    async fn doctor(&self) -> Doctor;
    fn validate_alias(&self, alias: &ModelAlias, entry: &ModelEntry) -> Result<(), ConfigError>; // worker-specific extra keys
    async fn start(&self, req: TaskAttemptRequest) -> Result<WorkerHandle, WorkerError>;        // spawn only; errors = cannot spawn
}
```

`WorkerError` (spawn-time): `BinaryMissing`, `InvalidAlias`, `WorkspaceUnavailable`,
`PolicyViolation` (dangerous flag outside container tier). Runtime failures
arrive as `WorkerEvent::Failed`, never as `Err`.

## Subprocess supervisor (`kevin_worker::supervisor`)

Shared by the four CLI adapters:

- `tokio::process::Command`, `kill_on_drop(true)`, `process_group(0)` (own
  pgid so the whole tree can be signalled), `current_dir(workspace.root)`,
  `env_clear()` then only allow-listed variables, plus `KEVIN_RUN_ID`,
  `KEVIN_TASK_ID`, `KEVIN_ATTEMPT_ID`, `KEVIN_WORKSPACE`.
- stdin: prompt written then closed (or passed as argument — per adapter).
- stdout: `BufReader::lines()` in a task; each line → adapter `parse_line` →
  zero or more `WorkerEvent`s; malformed lines are logged at debug and appended
  to the transcript only. Line length capped at 1 MiB (longer → truncated,
  counted in metrics). stderr: same reader, stored in transcript, surfaced as
  `Failed.message` tail (last 4 KiB) on non-zero exit.
- Send side is a bounded mpsc; when the consumer lags the reader awaits, the
  child blocks on the pipe → bounded memory by construction.
- Timeout: `tokio::time::timeout(budget.timeout)` around the wait; expiry →
  cancel → `Failed { class: Transient, message: "timeout" }`.
- Cancellation: `SIGTERM` to the process group, wait `workers.kill_grace`
  (default 10 s), then `SIGKILL`;
  outcome `Failed { class: Cancelled }`.
- Exit classification: exit 0 with a `Final` seen → `Succeeded`; exit 0 without
  `Final` → `Failed{Permanent, "no final message"}`; non-zero → `Transient`
  when stderr/exit matches known rate-limit/network patterns (429, `overloaded`,
  `ECONNRESET`, exit 137 = OOM/kill), `Permanent` otherwise; signal-killed by us
  → `Cancelled`/`Transient(timeout)`.
- Transcript: every raw line (stdout and stderr, tagged) is appended to
  `<data_dir>/runs/<run_id>/<task_id>/<attempt_id>.jsonl`; the orchestrator
  mirrors parsed events into `orch.task_log`. An `ArtifactRef{kind: Transcript}`
  is returned in the outcome.
- Metrics: the supervisor records `kevin_worker_processes{worker}`,
  `kevin_worker_exits_total{worker,class}` and
  `kevin_worker_spawn_duration_seconds{worker}`; tokens are
  `kevin_tokens_total{model_alias,direction}`, recorded by the task runner.
  The authoritative list (names, types and labels) is the table in
  [10](./10-observability-ops.md) §Metrics — that is the one the exporter and
  the `ac_ws20_4` test check against.

## Adapter: `claude` (Claude Code)

Command (one argv entry per line; cwd = workspace):

```text
claude -p
  --output-format stream-json --verbose
  --model <model.model>                         # e.g. claude-opus-5
  --permission-mode <workers.claude.permission_mode>
  --allowedTools <tool> <tool> …                # workers.claude.allowed_tools
  --append-system-prompt <context>              # Kevin briefing: task title, acceptance criteria, lessons
  [--json-schema <json>]                        # when spec.output_schema is set and structured_output = "json_schema"
  [--session-id <uuid>]                         # fresh attempt: attempt_id as uuid
  [--resume <session>]                          # follow-up attempt: context.prior_session
  [--max-turns <workers.claude.max_turns>]
  [--dangerously-skip-permissions]              # ONLY sandbox.tier = "container"
  <extra_args…>
```

Prompt is written to stdin (avoids argv length limits and shell quoting).

Event mapping (stream-json, one JSON object per line):

| stream-json line | WorkerEvent |
|---|---|
| `{"type":"system","subtype":"init","session_id",…}` | `Started{session_id}` |
| `{"type":"assistant","message":{"content":[{"type":"text","text"}]}}` | `AssistantText{delta}` |
| content block `{"type":"tool_use","name","input"}` | `ToolCall{name, input_summary = first 200 chars of input json}` |
| `{"type":"user","message":{"content":[{"type":"tool_result",…}]}}` | `ToolResult{ok = !is_error}` |
| `message.usage` on assistant lines | `Usage{delta}` (input/output/cache_read/cache_creation) |
| `{"type":"result","subtype":"success","result","total_cost_usd","usage","session_id","structured_output"?}` | `Final{text=result, structured, usage(cost_usd=total_cost_usd)}` |
| `{"type":"result","subtype":"error_max_turns" / "error_during_execution"}` | `Failed{Permanent / Transient}` |

Verified against `claude` **2.1.239** by a real capture
(`crates/kevin-worker/tests/fixtures/claude/success.jsonl`, argv and scrubbing
recorded in `success.meta.toml`): `system/init`, `assistant/thinking`,
`assistant/tool_use`, `user/tool_result` (incl. `is_error`), `assistant/text`,
`result/success` with `usage` + `total_cost_usd`, and the `rate_limit_event`,
`system/hook_started`, `system/hook_response`, `system/thinking_tokens` lines
the adapter ignores.

Still `[inferred — verify]`: `result.structured_output` (the `--json-schema`
path) and the `error_max_turns` / `error_during_execution` subtypes. All three
strings are present in the 2.1.239 binary and are pinned by hand-written
fixtures, but none has been observed live; `inferred.meta.toml` records exactly
which field of each fixture is inferred. What would settle them: a live capture,
`KEVIN_LIVE_TESTS=1 cargo nextest run -p kevin-worker live_ -- --run-ignored all`.

Effort: no CLI flag → effort is ignored for claude (model alias carries it).

## Adapter: `codex` (OpenAI Codex CLI)

```text
codex exec --json
  -m <model.model>
  -C <workspace.root>
  -s <workers.codex.sandbox>                    # read-only | workspace-write ; danger-full-access only container tier
  [--output-schema <data_dir>/runs/<run>/<task>/<attempt>.schema.json]   # spec.output_schema
  -o <data_dir>/runs/<run>/<task>/<attempt>.last.txt
  [-c model_reasoning_effort=<low|medium|high|xhigh|max>]   # Effort mapping 1:1
  --skip-git-repo-check
  [--ephemeral]                                 # opt-in; disables resume
  [--dangerously-bypass-approvals-and-sandbox]  # ONLY container tier
  <extra_args…>                                 # deduplicated against the above
  -                                             # prompt from stdin
```

`codex exec` has no `--append-system-prompt`: the Kevin briefing (title,
acceptance criteria, operator context, memory block) is prepended to the prompt
written on stdin.

Follow-up: `codex exec resume <session_id> --json … -` with the new prompt.
`resume` accepts neither `-C` nor `-s` (verified, `codex-cli` 0.149.0): the cwd
comes from the supervisor and the sandbox from the resumed session.

Event mapping (JSONL): `thread.started{thread_id}` → `Started{session_id}`;
`item.completed{item.type: "agent_message", text}` → `AssistantText`;
`item.completed{item.type: "reasoning", text}` → `Thinking`;
`item.started`/`item.completed` with `item.type` ∈ {`command_execution`,
`file_change`, `mcp_tool_call`, `web_search`, `todo_list`} →
`ToolCall`/`ToolResult` (`ok = false` on a non-zero `exit_code` or a `failed` /
`declined` status); `turn.completed{usage}` → `Usage` then `Final` (text =
contents of the `-o` file, falling back to the last `agent_message`; structured
= extracted from that text and validated, the stream carries no structured
field); `turn.failed{error.message}` / `{"type":"error","message"}` → `Failed`.
`turn.started` and `item.updated` are transcript-only. Fixtures pin the shapes.

Usage: `turn.completed.usage` is `{input_tokens, cached_input_tokens,
cache_write_input_tokens, output_tokens, reasoning_output_tokens}`;
`input_tokens` *includes* the cached ones (Kevin subtracts them so
`total_tokens()` does not double count) and `reasoning_output_tokens` is
already part of `output_tokens`. No cost anywhere → router price table.

## Adapter: `pi`

```text
pi -p --mode json
  --provider <model.provider>                   # required for pi aliases (validate_alias)
  --model <model.model>
  [--thinking <low|medium|high|xhigh|max>]      # Effort mapping 1:1 (pi also has off|minimal)
  --append-system-prompt <context>              # + the JSON-schema instruction, see Structured output
  [--tools read,grep,find,ls]                   # read-only, in-place attempts (09-security)
  [--session-id <attempt uuid>]                 # fresh attempt when sessions are kept
  [--session <session id>]                      # follow-up / repair turn; drops --no-session
  <extra_args…>                                 # default: --no-session (ephemeral)
  <message>                                     # prompt as final arg (pi has no stdin prompt)
```

Verified against `pi` 0.84.2. `--session-id` only *creates/names* a project
session; resuming one is `--session <path|id>`, so a follow-up uses `--session`
and drops the contradicting `--no-session`. An argv entry starting with `@` is
read as a file attachment and one starting with `-` is rejected, so a prompt
beginning with either is prefixed with a newline.

Event mapping for `--mode json` (documented by the CLI's own `docs/json.md` as
`JsonAgentSessionEvent`, pinned by fixtures): the `{"type":"session","id"}`
stream header → `Started{session_id}`; `message_update` whose
`assistantMessageEvent.type` is `text_delta` → `AssistantText` and
`thinking_delta` → `Thinking` (`toolcall_*` deltas are argument fragments and
are transcript-only); `tool_execution_start{toolName,args}` → `ToolCall`;
`tool_execution_end{toolName,result,isError}` → `ToolResult{ok = !isError}`;
`message_end` of an assistant message → `Usage{delta}`. `agent_start`,
`turn_*`, `auto_retry_*`, `queue_update` and `agent_end` are transcript-only.

`pi` has **no terminal line**: `agent_end` repeats once per internal auto-retry,
so the verdict comes from the *last* assistant `message_end`: `stopReason`
`stop` → `Final` (text = its `text` content blocks), `error` → `Failed`
(`Transient` when `errorMessage` matches the rate-limit/network signature, else
`Permanent`), `aborted` → `Failed{Cancelled}`, `length` → `Failed{Permanent}`,
`toolUse`/`pending` at end of stream → no final message. Print mode always
exits 0 — even on provider errors, which only `--mode text` turns into exit 1 —
so the exit status alone never classifies a `pi` attempt.

Usage: `message_end.message.usage` is the total of *that one message*
(`{input, output, cacheRead, cacheWrite, reasoning?, totalTokens, cost{…, total}}`,
camelCase), so the per-message totals add up. `pi` computes cost itself:
`usage.cost.total` fills `Usage.cost_usd` and the router price table is not
consulted. Structured output: prompt instruction + extraction (see below); the
repair turn resumes with `--session` when sessions are kept, otherwise it
restates the previous answer in a fresh, ephemeral turn.

## Adapter: `opencode`

```text
opencode run --format json
  -m <model.model>                              # provider/model, e.g. anthropic/claude-sonnet-5
  --dir <workspace.root>
  [--variant <effort>]                          # Effort → low|medium|high|max (XHigh→high)
  [--agent <workers.opencode.agent>]
  [-s <session_id>]                             # follow-up
  <extra_args…>
  <message>
```

`--auto` is a `PolicyViolation` unless `sandbox.tier = "container"`.

`opencode run` has neither a system-prompt flag nor an output-schema flag: the
Kevin briefing *and* the "respond with only a JSON object matching this schema"
instruction both ride in the trailing `<message>` positional. Nothing is read
from stdin.

Event mapping (verified, `opencode` 1.18.15). The emitter writes one
`{type, timestamp, sessionID, …payload}` object per line with
`type ∈ {step_start, tool_use, step_finish, text, reasoning, error}`:
`text` part → `AssistantText`; `reasoning` part → `Thinking` (only with
`--thinking`); `tool_use` → `ToolCall` **and** `ToolResult` on the same line
(the line is emitted once, already carrying the tool's terminal
`state.status ∈ {completed, error}`); `step_finish` → `Usage`; `error` →
`Failed`. `step_start` is transcript-only. Session id from the `sessionID` of
the first line → `Started`.

**There is no terminal line.** The emitter loop breaks when the session goes
idle and the process exits — 0 normally, 1 once a `session.error` was seen (and
stderr stays empty). The adapter therefore synthesises the single `Final` after
exit, treats "a step finished and no `error` line arrived" as `saw_final`, and
lets an `error` line override the generic exit-code verdict. `Final.text` is
the concatenated `text` parts of the *last* assistant message; `structured` is
extracted from it (falling back to the whole transcript text) and validated.

Usage: `step-finish.tokens` is `{total?, input, output, reasoning, cache:{read,
write}}` per step, with `total = input + output + reasoning + cache.read`;
`reasoning` is *not* part of `output`, so Kevin adds it to `output_tokens`
exactly as `opencode stats` does. `step-finish.cost` is a per-step USD amount,
so opencode is — with claude — a worker that reports cost and the router price
table is only a fallback.

Failure classes come from the shipped `NamedError` variants (`APIError`,
`ProviderAuthError`, `MessageAbortedError`, `StructuredOutputError`,
`ContextOverflowError`, `ContentFilterError`, `MessageOutputLengthError`):
`MessageAbortedError` → `Cancelled`; otherwise `error.data.isRetryable` decides,
then the HTTP status, then a rate-limit/network signature in the message.

Doctor: `opencode --version`, then auth offline — a provider API key in the
environment, else `$XDG_DATA_HOME/opencode/auth.json` (or
`~/.local/share/opencode/auth.json`), else the credential count printed by
`opencode providers list`, which only reads the local store.

## Adapter: `fake`

In-process, no subprocess. Driven by `workers.fake.script` (YAML):

```yaml
default: { reply: "done", usage: { input_tokens: 10, output_tokens: 5 } }
rules:
  - match: "reply deterministically"      # substring or /regex/
    reply: "kohral-ok"                     # Kohral conformance basic phase
  - match: "[[KOHRAL_HOLD]]"
    hold: true                             # emits Started then waits until cancelled (crash phases)
  - match: /implement .* auth/
    events: [ {tool_call: {name: edit, input_summary: "src/auth.rs"}}, {text: "Added auth"} ]
    structured: { status: "ok" }
    delay_ms: 50
  - match: "fail transient"
    fail: { class: transient, message: "simulated 429" }
```

First matching rule wins; `default` otherwise. The fake worker honours
cancellation and timeouts exactly like real ones, so orchestrator tests and the
Kohral conformance suite need no model.

## Structured output

1. If `spec.output_schema` is set: claude → `--json-schema`; codex →
   `--output-schema`; pi/opencode → append "Respond with only a JSON object
   matching this schema: …" to the context.
2. On `Final`, take `structured` if the CLI returned it; else extract the
   first balanced JSON object/array from `text` (fenced or bare).
3. Validate with the `jsonschema` crate. On failure: one repair attempt — a
   follow-up turn on the same session ("Your previous answer did not match the
   schema: <errors>. Reply with only corrected JSON"). Second failure →
   `Failed{Permanent, "schema_violation"}`.

## Usage, cost, effort, sessions, limits

| Worker | Usage source | Cost | Session / follow-up | Effort |
|---|---|---|---|---|
| claude | `message.usage`, `result.usage` | `total_cost_usd` from result | `--session-id` / `--resume` | none (alias) |
| codex | `turn.completed.usage` | price table | `codex exec resume <thread_id>` | `-c model_reasoning_effort` (1:1, `max` included) |
| pi | `message_end.message.usage` (per message, camelCase) | `usage.cost.total` from the same block | `--session-id` then `--session <id>` | `--thinking` (1:1) |
| opencode | `step-finish.tokens` per step (reasoning added to output) | `step-finish.cost` per step | `-s <id>` | `--variant` |
| fake | scripted | 0 | n/a | ignored |

Cost fallback: `Usage.cost_usd = None` from the worker → orchestrator asks
`kevin-router::PriceTable::cost(alias, usage)`; `None` if the alias has no
prices (e.g. `gpt56-codex`).

Limits: no per-turn streaming of tool output for codex/pi beyond what `--json`
provides; claude `--max-turns` is the only hard turn cap; workers' own
sandboxes are the security boundary in `cli-native` tier (see [09](./09-security.md)).
Timeouts come from `budget.default_task_wall` unless the task spec overrides.

## Registry and doctor

```rust
pub struct WorkerRegistry { map: HashMap<WorkerKind, Arc<dyn Worker>> }
impl WorkerRegistry {
    pub fn from_config(cfg: &KevinConfig, sandbox: SandboxPolicy) -> Result<Self, ConfigErrors>; // only enabled workers; runs validate_alias for every [models.*]
    pub fn get(&self, kind: WorkerKind) -> Option<Arc<dyn Worker>>;
    pub async fn doctor_all(&self) -> Vec<Doctor>;
}
```

`kevin workers doctor` prints one row per configured worker:
`claude  /Users/x/.local/bin/claude  2.1.x  auth: ready  models: opus5-claude, sonnet5-claude, …`,
`codex   missing (workers.codex.bin = "codex")  → disable or install`, and
exits 1 if any *enabled* worker is missing or a role alias is unusable.
Auth readiness — every probe is offline and spends nothing; all four are now
implemented and verified against the installed CLIs:

- claude → `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN`, else
  `~/.claude/.credentials.json`. `claude auth status` is deliberately **not**
  called (it can cost a request); when `~/.claude` exists without a credentials
  file the answer is `Unknown` with a hint, because macOS keeps the OAuth token
  in the keychain where Kevin cannot read it.
- codex → `OPENAI_API_KEY`, else `$CODEX_HOME/auth.json` (`$CODEX_HOME`
  defaulting to `~/.codex`).
- pi → `pi auth check --provider <p> --json --no-refresh` per configured alias
  provider; `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` also count.
- opencode → a provider API key (`OPENCODE_API_KEY`, `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `OPENROUTER_API_KEY`, `GEMINI_API_KEY`),
  `~/.local/share/opencode/auth.json`, or the credential count of
  `opencode providers list` (verified, 1.18.15).

## Testing

- `crates/kevin-worker/tests/fixtures/<kind>/*.jsonl` — golden stdout captures
  (success, tool use, error, rate-limit, schema output) per adapter; a parser
  test replays each and asserts the `WorkerEvent` sequence and final `Usage`.
- `tests/bin/fake-cli.rs` — a tiny binary shim compiled in dev that replays a
  fixture to stdout (optionally sleeping / exiting non-zero / ignoring SIGTERM);
  adapters are pointed at it via `workers.<kind>.bin` to test the supervisor:
  timeout, SIGTERM→SIGKILL, back-pressure, transcript writing, exit
  classification.
- `fake` worker unit tests for scenario matching, hold, cancellation.
- Contract test: every adapter passes the same `WorkerContractSuite`
  (start → Started first, exactly one terminal event, no events after
  terminal, cancellation within grace).
- Doctor tests with PATH manipulation.
- Live smoke tests behind `KEVIN_LIVE_TESTS=1` (ignored in CI) that run each
  real CLI with a trivial prompt and refresh the golden fixtures.
