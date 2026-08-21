# 03 — Configuration schema

Crate: `kevin-config`. Format: **TOML**. One typed model (`KevinConfig`,
serde + `#[serde(deny_unknown_fields)]`) resolved at startup, validated as a
whole (all errors reported together), then immutable for the process lifetime.

## Sources and precedence (lowest → highest)

1. Built-in defaults (`KevinConfig::default()` — the TOML below *is* the default).
2. User file: `$XDG_CONFIG_HOME/kevin/kevin.toml` (`~/.config/kevin/kevin.toml`).
3. Project file: `./.kevin/kevin.toml` (walks up from cwd to the repo root; first found wins).
4. Environment: `KEVIN__<SECTION>__<KEY>` (double underscore = nesting, e.g. `KEVIN__DATABASE__URL`). Secrets should come from env or `*_file` keys, never from files committed to a repo.
5. CLI flags (`--config <file>` adds a layer between 3 and 4; `--set section.key=value` is highest).

`kevin config show` prints the effective config with secrets redacted and each
value's source; `kevin config validate` exits non-zero on errors; `kevin config
init` writes a commented user file and a random token; `kevin config
rotate-token` replaces the token file.

## Full schema with defaults

```toml
# ~/.config/kevin/kevin.toml — every key shown with its default.

[kevin]
data_dir = "~/.local/share/kevin"     # artifacts, worker transcripts, embeddings cache
instance_name = "kevin"               # appears in logs/metrics; Kohral sets agent name
profile = "laptop"                    # laptop | server | kohral — only changes *defaults* below, never behaviour branches
auto_approve_plans = false            # true: skip plan approval (forced true in headless/kohral runs)
shutdown_grace_period = "30s"

[database]
url = "postgres://kevin:kevin@localhost:5432/kevin"   # KEVIN__DATABASE__URL ; or url_file = "/run/secrets/db-url"
pool_size = 10
auto_migrate = true                   # server/kohral profiles default false
statement_timeout = "30s"

[server]
enabled = true                        # `kevin run` starts an ephemeral server when no server is configured
bind = "127.0.0.1:7777"
auth_token_file = "~/.config/kevin/token"   # created by `kevin config init`; Kohral mounts its own
cors_origins = []
request_timeout = "30s"
sse_keepalive = "15s"
docs = true                           # Swagger UI at /api/v1/docs (laptop profile default; server/kohral default false)
token_grace = "5m"                    # old token accepted this long after `kevin config rotate-token` + SIGHUP

[client]
server_url = ""                       # empty → `kevin run/tui` auto-start an embedded runtime; set to use a remote daemon
token_file = "~/.config/kevin/token"

[budget]
default_run_usd = 10.0
default_task_usd = 3.0
default_run_wall = "2h"
default_task_wall = "30m"
max_attempts = 2
max_parallel_tasks = 4                # global bulkhead for worker subprocesses
max_tokens_per_task = 2_000_000       # soft: counts input+output reported by workers

[orchestrator]
question_confidence_threshold = 0.7   # proposed questions with confidence_if_unasked below this become real Questions
max_questions_per_run = 4
max_tasks_per_run = 24
role_call_timeout = "15m"             # planner / judge / integrator worker calls
question_default_timeout = "10m"      # headless/Kohral: apply default after this; interactive: block
plan_revision_limit = 2               # RejectPlan → re-plan cycles before failing
evaluation_timeout = "10m"            # run completes with evaluation skipped after this
progress_interval = "10s"             # min interval between task.progressed events per attempt

[concurrency]
worker_threads = 0                    # 0 = num_cpus
per_worker_kind = { claude = 4, codex = 4, pi = 4, opencode = 4, fake = 64 }
blocking_threads = 2                  # embeddings etc.

# ---------------------------------------------------------------- workers
[retention]
task_log_days = 30                    # orch.task_log rows; `kevin db prune`
transcript_days = 30                  # raw worker transcripts under data_dir
artifact_days = 90

[workers]
kill_grace = "10s"                    # SIGTERM → SIGKILL delay on cancel/timeout (all adapters)

[workers.claude]
enabled = true
bin = "claude"
permission_mode = "acceptEdits"       # plan | acceptEdits | default ; `bypassPermissions` only allowed when sandbox.tier = "container"
allowed_tools = ["Read","Edit","Write","Bash(git *)","Bash(cargo *)","Bash(npm *)","Bash(pnpm *)","Bash(bun *)","Grep","Glob"]
extra_args = []
env_passthrough = ["ANTHROPIC_API_KEY","CLAUDE_CODE_OAUTH_TOKEN","HOME","PATH","SSL_CERT_FILE"]
max_turns = 200
structured_output = "json_schema"     # uses --json-schema when a TaskSpec.output_schema exists

[workers.codex]
enabled = true
bin = "codex"
sandbox = "workspace-write"           # read-only | workspace-write | danger-full-access (container tier only)
extra_args = ["--skip-git-repo-check"]
env_passthrough = ["OPENAI_API_KEY","CODEX_HOME","HOME","PATH"]

[workers.pi]
enabled = true
bin = "pi"
extra_args = ["--no-session"]
env_passthrough = ["HOME","PATH","ANTHROPIC_API_KEY","OPENAI_API_KEY","GEMINI_API_KEY"]

[workers.opencode]
enabled = true
bin = "opencode"
agent = ""                            # optional `--agent <name>` (OpenCode agent definition); empty = default
extra_args = []
env_passthrough = ["HOME","PATH","ANTHROPIC_API_KEY","OPENAI_API_KEY"]

[workers.fake]
enabled = false                       # tests & Kohral conformance set true
script = ""                           # path to a YAML/JSON scenario (see 04-workers)

# ---------------------------------------------------------------- model catalog
# Aliases are the routing vocabulary. Prices in USD per 1M tokens; used for cost
# accounting when a worker doesn't report cost. Unknown price → cost = null.
[models.opus5-claude]
worker = "claude"
model = "claude-opus-5"
tier = "frontier"
context_tokens = 1_000_000
input_usd_per_m = 5.00
output_usd_per_m = 25.00
tags = ["reasoning","coding","planning","judge"]

[models.fable5-claude]
worker = "claude"
model = "claude-fable-5"
tier = "frontier"
context_tokens = 1_000_000
input_usd_per_m = 10.00
output_usd_per_m = 50.00
tags = ["reasoning","planning","judge","hard"]

[models.sonnet5-claude]
worker = "claude"
model = "claude-sonnet-5"
tier = "balanced"
context_tokens = 1_000_000
input_usd_per_m = 3.00
output_usd_per_m = 15.00
tags = ["coding","implement","test","review"]

[models.haiku45-claude]
worker = "claude"
model = "claude-haiku-4-5"
tier = "fast"
context_tokens = 200_000
input_usd_per_m = 1.00
output_usd_per_m = 5.00
tags = ["summarise","classify","cheap"]

[models.gpt56-codex]
worker = "codex"
model = "gpt-5.6"                     # [inferred — verify current Codex default model id]
tier = "frontier"
tags = ["coding","implement","review"]
# prices unknown → leave unset; cost accounting reports null for this alias

[models.sonnet5-pi]
worker = "pi"
provider = "anthropic"                # worker-specific extra key (pi needs provider + model)
model = "claude-sonnet-5"
tier = "balanced"
input_usd_per_m = 3.00
output_usd_per_m = 15.00
tags = ["coding"]

[models.sonnet5-opencode]
worker = "opencode"
model = "anthropic/claude-sonnet-5"
tier = "balanced"
input_usd_per_m = 3.00
output_usd_per_m = 15.00
tags = ["coding"]

[models.fake]
worker = "fake"
model = "fake"
tier = "fast"
input_usd_per_m = 0
output_usd_per_m = 0

# ---------------------------------------------------------------- roles
[roles]
planner = "opus5-claude"              # understanding + plan
clarifier = "opus5-claude"            # question drafting (same worker call as planner by default)
judge = "opus5-claude"                # evaluation
integrator = "sonnet5-claude"         # merge/integration step
default = "sonnet5-claude"            # fallback when routing has no candidates
effort = { planner = "xhigh", judge = "high", integrator = "medium" }

# ---------------------------------------------------------------- routing
[routing]
policy = "thompson"                   # thompson | epsilon_greedy | fixed
exploration = 0.10                    # epsilon for epsilon_greedy; floor on exploration for thompson
min_samples_before_exploit = 3
quality_weight = 0.7                  # score = w_q*quality + w_c*(1-norm_cost) + w_l*(1-norm_latency)
cost_weight = 0.2
latency_weight = 0.1
prefer_tier_for_complexity = { low = "fast", medium = "balanced", high = "frontier" }

[routing.kinds.implement]
candidates = ["sonnet5-claude","gpt56-codex","opus5-claude"]
[routing.kinds.test]
candidates = ["sonnet5-claude","gpt56-codex"]
[routing.kinds.review]
candidates = ["opus5-claude","gpt56-codex"]
[routing.kinds.research]
candidates = ["opus5-claude","sonnet5-claude"]
[routing.kinds.write]
candidates = ["sonnet5-claude","haiku45-claude"]
[routing.kinds.debug]
candidates = ["opus5-claude","gpt56-codex","sonnet5-claude"]
[routing.kinds.refactor]
candidates = ["sonnet5-claude","gpt56-codex"]
[routing.kinds.ops]
candidates = ["sonnet5-claude"]
# understand/clarify/plan/evaluate/integrate use [roles], not routing.

# ---------------------------------------------------------------- memory
[memory]
enabled = true
embedder = "fastembed"                # fastembed (local ONNX) | none ; HTTP embedders are a later extension
embedding_model = "BAAI/bge-small-en-v1.5"   # 384 dims; changing it requires `kevin memory reindex`
dimensions = 384
top_k = 8
min_similarity = 0.35
context_max_tokens = 2500             # cap of the rendered <kevin-memory> block injected into planner/worker context
store_run_summaries = true
store_artifact_summaries = true
decay_half_life_days = 90             # importance decay for ranking, never deletion

# ---------------------------------------------------------------- evaluation
[evaluation]
enabled = true
evaluate_tasks = true                 # per-task judge pass (costs money) — false evaluates only the run
rubric = "default"                    # built-in rubrics: default | code | research | writing ; or path to a TOML rubric
auto_apply = ["routing","memory"]     # what evaluations may change without a human: routing scores, memory/lessons
proposals_require_approval = true     # prompt/config proposals are always just proposals

# ---------------------------------------------------------------- workspace & sandbox
[workspace]
strategy = "auto"                     # auto (jj if .jj exists, else git worktree, else in_place) | git_worktree | jj_workspace | in_place
root = ".kevin/workspaces"            # relative to the target repo; added to .git/info/exclude (jj: repo-local ignore) on first use
branch_prefix = "kevin/"
cleanup = "on_success"                # on_success | always | never
integration = "pr"                    # pr | merge | none (leave branches)
pr_per_task = false                   # pr mode: one PR per succeeded task instead of one integrated PR

[checks]
commands = []                         # repo checks run by the integrator before opening a PR, e.g. ["just ci"]; allowed in the project layer

[sandbox]
tier = "cli-native"                   # cli-native | container | none
allow_dangerous_flags = false         # derived: true only when tier = "container"
network = "inherit"                   # inherit | deny (container tier only)
env_allowlist_extra = []

# ---------------------------------------------------------------- telemetry
[telemetry]
log_format = "json"                   # json | pretty (tui/laptop default pretty)
log_level = "info"
metrics_bind = ""                     # "" disables the Prometheus exporter; Kohral profile: "0.0.0.0:9464"
otlp_endpoint = ""

# ---------------------------------------------------------------- kohral
[kohral]
enabled = false                       # `kevin serve --kohral` or profile = "kohral"
bind = "0.0.0.0:8080"
token_file = "/run/secrets/kohral-runtime-token"
identity_file = "/run/secrets/kohral-agent-identity"
collaboration_url = ""                # KOHRAL_COLLABORATION_URL
soul_file = "/opt/kevin/config/SOUL.md"
documentation_file = "/opt/kevin/config/KOHRAL_DOCUMENTATION.md"
memory_file = "/opt/kevin/data/MEMORY.md"
run_timeout = "30m"
max_attachment_bytes = 26_214_400     # 25 MiB per temporary attachment
```

## Validation rules (fail startup)

- `database.url` parses as `postgres://`; exactly one of `url`/`url_file`.
- Every `[roles.*]` and every `routing.kinds.*.candidates[]` entry names an
  existing `[models.<alias>]` whose `worker` is `enabled`.
- `[models.*].worker` ∈ known `WorkerKind`; `pi` aliases require `provider`.
- `workers.claude.permission_mode = "bypassPermissions"`,
  `workers.codex.sandbox = "danger-full-access"` or `opencode --auto` in
  `extra_args` are rejected unless `sandbox.tier = "container"`.
- Durations parse (`humantime`); budgets are > 0; `max_parallel_tasks ≥ 1`;
  `memory.dimensions` matches the chosen embedder model's known dimension.
- `kevin.profile` only selects defaults: `server` → `database.auto_migrate=false`,
  `telemetry.log_format=json`, `server.docs=false`; `kohral` → also `kohral.enabled=true`,
  `kevin.auto_approve_plans=true`, `server.bind=0.0.0.0:7777`,
  `workers.fake.enabled` stays false unless set.
- Project-layer files (`./.kevin/kevin.toml`) may not set `sandbox.*`, `workers.*`,
  `server.*`, `database.*` or `kohral.*` (`ConfigError::ProjectLayerNotAllowed`) —
  a cloned repo must not be able to weaken the sandbox or redirect the runtime.
- `server.bind` on a non-loopback address requires a non-empty auth token file
  (`ConfigError::InsecureBind`); the Kohral profile satisfies this via `kohral.token_file`.
- Unknown keys anywhere are errors (`deny_unknown_fields`), except under
  `[models.<alias>]` where worker-specific extras are allowed and validated by
  the worker adapter (`Worker::validate_alias`).

## Environment variables summary

| Variable | Maps to |
|---|---|
| `KEVIN__DATABASE__URL` | `database.url` |
| `KEVIN__SERVER__BIND` | `server.bind` |
| `KEVIN__KEVIN__PROFILE` | `kevin.profile` |
| `KEVIN_CONFIG` | extra config file path (same as `--config`) |
| `KOHRAL_COLLABORATION_URL`, `KOHRAL_RUNTIME_TOKEN_FILE` | read by the Kohral profile as aliases of `kohral.*` |

## Rust shape

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct KevinConfig { pub kevin: General, pub database: Database, pub server: Server, pub client: Client,
    pub budget: Budget, pub concurrency: Concurrency, pub workers: Workers, pub models: BTreeMap<ModelAlias, ModelEntry>,
    pub roles: Roles, pub routing: Routing, pub memory: Memory, pub evaluation: Evaluation,
    pub workspace: WorkspaceCfg, pub sandbox: Sandbox, pub telemetry: Telemetry, pub kohral: Kohral }

pub fn load(opts: LoadOptions) -> Result<Resolved, ConfigErrors>; // Resolved { config, sources: BTreeMap<String, Source> }
pub struct ConfigErrors(pub Vec<ConfigError>);                    // all errors at once, with key path + source
```

Loading uses the `figment` crate (TOML + env + serialized defaults) or an
equivalent hand-rolled layering; the `sources` map is required for `kevin
config show`.
