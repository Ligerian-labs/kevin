# Deploying Kevin

Kevin is one binary (`kevin`) plus a Postgres 16 database with the `pgvector`
extension. Everything else — how you get the binary, whether it runs in the
foreground or under a supervisor, how it is isolated — is deployment detail.
This directory holds the artefacts for the three topologies in
[`plan/01-architecture.md`](../plan/01-architecture.md).

| File | What it is |
|---|---|
| `compose/postgres.yml` | Postgres 16 + pgvector for development and tests (`just db-up`). |
| `Dockerfile` | The **Kevin daemon** image: multi-stage build, `kevin serve`, no agent CLIs. |
| `kohral/` | The Kohral image and stack (WS-23) — bundles the agent CLIs. Not in this repository yet. |

## Which topology?

### Laptop

Run the binary directly. Postgres comes from `compose/postgres.yml`.

```bash
just db-up                                  # pgvector on localhost:5433
kevin config init                           # writes ~/.config/kevin/kevin.toml + a token
export KEVIN__DATABASE__URL="postgres://kevin:kevin@localhost:5433/kevin"
kevin db init && kevin db migrate
kevin run "add a health endpoint to the API"
```

The `laptop` profile defaults to `database.auto_migrate = true` and binds the
API on `127.0.0.1:7777` for the ephemeral in-process server. Nothing listens on
a public interface.

### VPS

Same binary, run as a long-lived daemon under systemd, with clients (`kevin
tui --server …`, `kevin runs …`) talking to it over HTTP.

```ini
# /etc/systemd/system/kevin.service
[Unit]
Description=Kevin agent runtime
After=network-online.target postgresql.service

[Service]
User=kevin
Environment=KEVIN__KEVIN__PROFILE=server
Environment=KEVIN__SERVER__BIND=127.0.0.1:7777
EnvironmentFile=/etc/kevin/env          # KEVIN__DATABASE__URL lives here, mode 0600
ExecStartPre=/usr/local/bin/kevin db migrate
ExecStart=/usr/local/bin/kevin serve
Restart=on-failure
TimeoutStopSec=60                       # > kevin.shutdown_grace_period (30s)
StateDirectory=kevin                    # /var/lib/kevin = kevin.data_dir

[Install]
WantedBy=multi-user.target
```

Notes that matter on a VPS:

- The `server` profile turns `database.auto_migrate` off. Run `kevin db migrate`
  explicitly (as above) so an upgrade cannot start writing before the schema is
  ready — see the migrations policy in
  [`plan/10-observability-ops.md`](../plan/10-observability-ops.md).
- Bind to loopback and put TLS and access control in a reverse proxy. The API's
  own auth is the bearer token in `server.auth_token_file`.
- `TimeoutStopSec` must exceed `kevin.shutdown_grace_period` (30 s by default)
  plus `workers.kill_grace` (10 s), or systemd will `SIGKILL` Kevin while
  attempts are still draining.
- The agent CLIs (`claude`, `codex`, `pi`, `opencode`) must be installed and
  authenticated **for the `kevin` user** — Kevin never holds provider
  credentials itself.

### Container

```bash
podman build -f deploy/Dockerfile -t kevin:dev .          # from the repository root
podman run --rm -it \
  -e KEVIN__DATABASE__URL="postgres://kevin:kevin@db:5432/kevin" \
  -e KEVIN__DATABASE__AUTO_MIGRATE=true \
  -v kevin-data:/var/lib/kevin \
  -p 7777:7777 \
  ghcr.io/ligerian-labs/kevin:latest
```

Published images: `ghcr.io/ligerian-labs/kevin` — `X.Y.Z`, `X.Y` and `latest`,
multi-arch (`linux/amd64`, `linux/arm64`), with an SBOM, SLSA provenance and a
keyless cosign signature. Verification commands are in
[`docs/releasing.md`](../docs/releasing.md).

**What the daemon image contains:** the `kevin` binary, `git` (task attempts get
a worktree each), `ca-certificates` and `curl` (only for `HEALTHCHECK`). It runs
as the unprivileged `kevin` user.

**What it does not contain, on purpose:** the coding-agent CLIs and their
runtimes, `jj`, and Postgres. This image orchestrates; it does not host workers.
An agent task scheduled inside it fails with a missing-binary diagnosis from
`kevin workers doctor`. Use the Kohral image (`deploy/kohral/`, WS-23) — or
derive your own image from this one — when the container must also *run* agents.

## Configuration surface

Kevin reads TOML with an environment overlay: `KEVIN__<SECTION>__<KEY>`, double
underscore for nesting. The full schema is
[`plan/03-config-schema.md`](../plan/03-config-schema.md); what a deployment
normally has to set:

| Variable | Required | Default in the image | Why |
|---|---|---|---|
| `KEVIN__DATABASE__URL` | **yes** | — | Postgres 16 + pgvector connection string. Prefer `KEVIN__DATABASE__URL_FILE=/run/secrets/db-url` so the secret never shows up in `docker inspect`. |
| `KEVIN__DATABASE__AUTO_MIGRATE` | no | `false` (`server` profile) | `true` lets the process migrate on startup; leave it off and run `kevin db migrate` as a pre-start step when several replicas exist. |
| `KEVIN__KEVIN__DATA_DIR` | no | `/var/lib/kevin` | Artifacts, worker transcripts, embedding-model cache. |
| `KEVIN__KEVIN__PROFILE` | no | `server` | `laptop` \| `server` \| `kohral`; only changes defaults. |
| `KEVIN__SERVER__BIND` | no | `0.0.0.0:7777` | The image overrides the `127.0.0.1` default, which would be unreachable from outside the container. |
| `KEVIN__SERVER__AUTH_TOKEN_FILE` | recommended | `~/.config/kevin/token` | Mount a token file; the API is not open by default. |

### Ports

| Port | Protocol | What |
|---|---|---|
| `7777` | HTTP | The API (`/api/v1/**`, SSE streams), health (`/healthz`, `/readyz`) and, when `telemetry.metrics` is enabled, `/metrics`. The only port the image exposes. |

Nothing else listens. Postgres is a client connection, not a served port.

### Volumes

| Path | Contents | If you lose it |
|---|---|---|
| `/var/lib/kevin` (`kevin.data_dir`) | Artifact copies, worker transcripts, the embedding-model cache. | Recoverable: transcripts and artifact copies are convenience, the model re-downloads on first use. Back it up if you care about transcripts beyond `retention.transcript_days`. |
| Postgres data | Events, read models, memory, route scores, evaluations. | **Not recoverable.** Postgres is the source of truth — back it up with `pg_dump --format=custom` or platform snapshots. Restore procedure: restore the dump, `kevin db status`, `kevin db rebuild-projection --all`, start. |

The image declares `VOLUME ["/var/lib/kevin"]`, so an anonymous volume appears
if you do not mount a named one. Mount one deliberately: the embedding model is
a ~130 MB download you do not want to repeat on every container restart.

## Health and lifecycle

- `GET /healthz` — liveness. Never touches the database; use it for the
  container/`systemd` liveness probe. That is what the image's `HEALTHCHECK`
  calls.
- `GET /readyz` — readiness: pool connected, migrations at the expected
  version, startup finished, not draining. Use it for the load-balancer probe
  and for rollout gating; a pod that is live but not ready is the expected
  state while migrations are pending.
- `POST /api/v1/maintenance/drain` — stop admitting new runs while in-flight
  attempts finish. Drain before every upgrade.
- `SIGTERM` starts the graceful shutdown: unready → stop scheduling →
  `kevin.shutdown_grace_period` for running attempts → kill process groups →
  flush → exit.
