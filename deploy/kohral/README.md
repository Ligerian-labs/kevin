# Kevin as a Kohral agent runtime — image & stack

This directory is what Kohral deploys when an operator picks the `kevin`
runtime: a container image that runs `kevin serve --kohral` **with the
coding-agent CLIs Kevin drives**, plus the per-agent stack around it.

| File | What it is |
|---|---|
| `Dockerfile` | The Kohral runtime image (multi-stage: Rust builder → model cache → Debian trixie runtime). |
| `entrypoint.sh` | Validate secrets → migrate → seed `MEMORY.md` → `exec kevin serve --kohral`. |
| `compose.yml` | The per-agent stack (`gateway` + `memory`), the Compose rendering of `KevinRuntimeStrategy::buildSpec()`. |
| `compose.conformance.yaml` | The same stack in the conformance profile, for `contract.py`. |
| `conformance.toml` | The config fragment that turns the fake worker on (`plan/08` §1.9). |
| `conformance-scenario.json` | The fake-worker scenario; generated from `kevin_kohral::conformance::scenario()`. |
| `upstreams.lock.json` | Every external pin: base images, Node, the four CLIs, the embedding model. |

Reference: [`plan/08-kohral-runtime.md`](../../plan/08-kohral-runtime.md) §5–§8,
[`plan/09-security.md`](../../plan/09-security.md) §Sandbox tiers,
[`plan/10-observability-ops.md`](../../plan/10-observability-ops.md) §Health and drain.

## What is in the image

* `kevin` — release build, `--locked`, stripped, non-root.
* **Node 24 and the four agent CLIs** — this is the difference from
  `deploy/Dockerfile`, which is the plain daemon and deliberately ships none of
  them. Versions come from `upstreams.lock.json`:
  `@anthropic-ai/claude-code`, `@openai/codex`, `opencode-ai`,
  `@earendil-works/pi-coding-agent`.
* `git`, `bash`, `curl`, `ca-certificates`, `openssh-client`, `jq`, `python3`,
  `ripgrep` — what the CLIs shell out to.
* The **fastembed model cache** for `BAAI/bge-small-en-v1.5`, downloaded at a
  pinned Hugging Face revision with per-file sha256 verification, so a fresh
  agent does not spend its first turn pulling 130 MB. Build with
  `--build-arg BAKE_EMBEDDING_MODEL=0` to leave it out (the model is then
  fetched on first use, or memory degrades to lexical search if there is no
  egress). The entrypoint copies it onto the data volume once, because the
  cache directory has to stay writable.
* No credentials, ever. Provider keys arrive at runtime as
  `/run/secrets/kevin-env` and are sourced by the entrypoint.

Layout:

| Path | Mount | Content |
|---|---|---|
| `/opt/kevin/config/` | read-only config files | `kevin.toml` (operator overlay), `AGENTS.md`, `SOUL.md`, `KOHRAL_DOCUMENTATION.md` |
| `/opt/kevin/data/` | volume `data` (uid 10000) | `data_dir`, `MEMORY.md`, `home/` (CLI state: `~/.claude`, `~/.codex`, `~/.pi`, `~/.config/opencode`), `work/`, `embeddings/`, `api-token` |
| `/run/secrets/kohral-runtime-token` | secret | the bearer token every `/v1` endpoint checks |
| `/run/secrets/kevin-database-url` | secret | the Postgres DSN, read through `KEVIN__DATABASE__URL_FILE` |
| `/run/secrets/postgres-password` | secret | used by the `memory` service, and by the entrypoint's fallback URL composition |
| `/run/secrets/kevin-env` | secret env file | provider keys for the CLIs; sourced, never logged |
| `/run/secrets/kohral-agent-identity` | secret | collaboration identity (phase 2) |

Ports: `8080` is the Kohral runtime contract and the only one exposed. The
operator API (`server.bind`, 7777) and the Prometheus exporter
(`telemetry.metrics_bind`, 9464) stay inside the stack network. The operator
API has its own bearer token, minted onto the data volume on first boot — the
operator API and the platform contract never share credentials.

### Two start paths

The image's `ENTRYPOINT` is what a hand-run stack and the conformance job get.
**Kohral replaces it**: `KevinRuntimeStrategy` gives the gateway service a
`/bin/sh -c "<seed MEMORY.md>; exec kevin serve --kohral"` command, because
Kohral owns the `MEMORY.md` seed (asserted by its
`KohralPlatformBriefingTest`). Both paths end in the same `kevin serve
--kohral`, and everything the entrypoint does is either idempotent or covered
by the image environment — which is why the image sets
`database.auto_migrate = true`: migrations must also happen on the path where
the entrypoint never runs.

## The stack

`gateway` (this image, port 8080, depends on `memory`, cpu 1 / mem 2G, volume
`data`, the secrets above) + `memory` (`pgvector/pgvector:pg16`, volume
`memory-data`, `pg_isready` healthcheck, cpu 0.5 / mem 1G), on one private
network — one isolated stack per agent, exactly like Hermes.

```sh
# build
podman build -f deploy/kohral/Dockerfile -t kevin-kohral:dev .

# secrets (see the header of compose.yml)
mkdir -p deploy/kohral/secrets
openssl rand -hex 32 > deploy/kohral/secrets/kohral-runtime-token
openssl rand -hex 32 > deploy/kohral/secrets/postgres-password
printf 'postgres://kevin:%s@memory:5432/kevin' \
  "$(cat deploy/kohral/secrets/postgres-password)" \
  > deploy/kohral/secrets/kevin-database-url
printf 'ANTHROPIC_API_KEY=%s\n' "$ANTHROPIC_API_KEY" > deploy/kohral/secrets/kevin-env

KEVIN_IMAGE=kevin-kohral:dev podman compose -f deploy/kohral/compose.yml up -d
curl -fsS http://127.0.0.1:8080/health
```

`deploy/kohral/secrets/` is git-ignored. Nothing in this directory is a real
credential.

> Podman's default OCI image format drops `HEALTHCHECK` (it warns at build
> time); the Compose files declare the same probe, so the stack still has one.
> Build with `--format docker` if you want the probe baked into the image.

## Sandbox tier: the trust argument, and its limits

The image sets `sandbox.tier = "container"` and
`workers.claude.permission_mode = "bypassPermissions"`. The argument
(`plan/09-security.md`): unattended turns cannot answer an approval prompt, and
the *stack* is the isolation boundary — a restricted namespace, no engine
socket, no host mounts, whatever egress policy Kohral applies, a non-root uid
10000, and a database only this agent can reach. Within that boundary, asking
a worker to also sandbox itself buys little and costs every turn.

What that argument does **not** buy, and what the deployment must keep in mind:

* every worker can read everything on the data volume — including the CLI auth
  state under `$HOME` and other runs' transcripts and workspaces. Secrets are
  therefore limited to what this agent legitimately needs; there is no
  per-task credential isolation inside the container.
* egress is whatever Kohral permits. A worker that can reach the internet can
  exfiltrate anything it can read.
* `codex` deliberately stays at `-s workspace-write`; the image never enables
  `danger-full-access`, `--dangerously-bypass-approvals-and-sandbox` or
  `--dangerously-bypass-hook-trust`. `FORBIDDEN_FLAGS` is still checked against
  the final argv of every subprocess.
* the tier is set by the *image environment*, not by configuration an operator
  or a checked-out repository can reach: Kohral protects the `sandbox` section,
  and the project layer (`./.kevin/kevin.toml`) may not set `sandbox.*` or
  `workers.*` at all. Outside this image the default stays `cli-native`, where
  the same flags are a config error.

## Operator overlay and protected sections

The agent's native runtime configuration (Kohral's guided fields + advanced
JSON editor) is deep-merged as a TOML fragment over Kevin's defaults and
mounted at `/opt/kevin/config/kevin.toml`.

**Protected** — rejected by `KevinRuntimeStrategy::validateConfiguration()`:
`server`, `kohral`, `database`, `sandbox`, `workers` (the whole section) and
`telemetry`. These decide how the runtime is reachable, isolated and observed;
they belong to the image and the control plane.

**Allowed**: `kevin.auto_approve_plans` (forced true in Kohral mode anyway),
`budget.*`, `models.*`, `roles.*`, `routing.*`, `memory.*`, `evaluation.*`,
`workspace.*`, `concurrency.*`. Unknown keys are rejected
(`deny_unknown_fields`), so a typo surfaces at rollout time rather than at the
first turn.

## Conformance

The suite is Kohral's own `runtime/conformance/contract.py --runtime hermes` —
never a vendored copy, so Kevin is always judged by Kohral's current
assertions. Against the built image:

```sh
podman build -f deploy/kohral/Dockerfile -t kevin-kohral:dev .
mkdir -p deploy/kohral/secrets
openssl rand -hex 32 > deploy/kohral/secrets/kohral-runtime-token
KEVIN_IMAGE=kevin-kohral:dev podman compose -f deploy/kohral/compose.conformance.yaml up -d

C=~/workspace/kohral/runtime/conformance/contract.py
T=$(cat deploy/kohral/secrets/kohral-runtime-token)
U=http://127.0.0.1:8080

python3 "$C" basic        --runtime hermes --base-url "$U" --token "$T"
python3 "$C" accept-crash --runtime hermes --base-url "$U" --token "$T" --run-id-file run.id
podman compose -f deploy/kohral/compose.conformance.yaml kill -s SIGKILL gateway
podman compose -f deploy/kohral/compose.conformance.yaml up -d gateway
python3 "$C" verify-crash --runtime hermes --base-url "$U" --token "$T" --run-id-file run.id

podman compose -f deploy/kohral/compose.conformance.yaml down -v
```

`kevin kohral conformance` runs the same three phases against an embedded
gateway (no container) and is what `crates/kevin-kohral` tests in-process; the
commands above are the container-level proof. CI runs them in
`.github/workflows/kohral-conformance.yml`, which **skips** (never fails) when
the private Kohral checkout is unavailable. `just ci` starts no container.

## Pins and updates

`upstreams.lock.json` records every external artefact with a digest: base
images, the Node tarball (sha256 per architecture), each CLI (npm package,
exact version, registry integrity digest) and the embedding model (repo,
revision, sha256 per file). The Dockerfile verifies what it downloads.

Update procedure — once per Kevin release, never on an unattended schedule
(Kohral's discipline, `runtime/README.md`):

1. Read the upstream release notes and advisories for the CLI you are moving.
2. Update the one entry in `upstreams.lock.json` **and** the matching `ARG` in
   the Dockerfile — `ac_ws23_1` fails if they disagree.
3. `cargo nextest run -p kevin-kohral` (pins, config fragment, scenario).
4. Rebuild the image and run the conformance suite above.
5. `kevin workers doctor` inside the image to confirm each CLI still reports a
   version and an auth status Kevin understands.
6. Release image and lock as one set.

Versions are the ones Kevin's worker adapters were written against
(`plan/04-workers.md`). A CLI outside the tested range is a `warn` in
`kevin workers doctor`, not a failure — but the mapping of its JSON output is
exactly the thing that breaks silently, so treat a major bump as adapter work.

## Upgrade and rollback

Kohral deployments reference **digests**, never tags (`plan/08` §8; release
images are built with SBOM + provenance and signed with Cosign).

* **Upgrade**: Kohral marks the deployment `draining`
  (`POST /v1/maintenance/drain`), which makes new turns 503 `gateway_draining`
  while in-flight ones finish; queued-but-unsubmitted turns stay in Kohral's
  Postgres for the replacement. Then the gateway is replaced with the new
  digest. Migrations are forward-only and run at boot
  (`database.auto_migrate = true`); the `memory` service and its volume are
  untouched.
* **If the drain expires** with work still accepted, killing the container
  leaves those runs non-terminal — the next boot's sweep fails them with
  `error_code = runtime_restarted`, preserving their partial output, and Kevin
  never replays accepted work. The operator retries them as a *new* turn after
  reading what was preserved.
* **Rollback**: restore the previous digest. Kevin's schema changes are
  additive and are not destructively downgraded, so an older gateway runs
  against the migrated database; if a release ever needs a
  non-forward-compatible migration it must say so in its notes and the
  rollback then means restoring the database snapshot too.
* The `data` volume survives both. It carries `MEMORY.md`, the CLI auth state
  and the embedding cache — losing it costs the agent its memory, not its
  configuration.
