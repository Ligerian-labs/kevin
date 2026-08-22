# Running Kevin under systemd

`kevin.service` is the VPS topology of
[`plan/01-architecture.md`](../../plan/01-architecture.md): one long-lived
`kevin serve` process, clients (`kevin tui --server …`) over HTTP, TLS
terminated by a reverse proxy.

## Install

```bash
# 1. an unprivileged user that owns nothing but its state directory
sudo useradd --system --home-dir /var/lib/kevin --shell /usr/sbin/nologin kevin

# 2. the binary and the unit
sudo install -m 0755 kevin /usr/local/bin/kevin
sudo install -m 0644 deploy/systemd/kevin.service /etc/systemd/system/kevin.service

# 3. configuration and secrets, readable by nobody else
sudo install -d -m 0750 -o kevin -g kevin /etc/kevin
sudo -u kevin kevin config init                 # kevin.toml + a fresh token, mode 0600
sudo install -m 0600 -o kevin -g kevin /dev/null /etc/kevin/kevin.env
sudo tee /etc/kevin/kevin.env >/dev/null <<'ENV'
KEVIN__DATABASE__URL=postgres://kevin:…@localhost:5432/kevin
KEVIN__SERVER__AUTH_TOKEN_FILE=/etc/kevin/token
ENV

# 4. the database, then the daemon
sudo -u kevin kevin db init
sudo systemctl daemon-reload
sudo systemctl enable --now kevin
curl -sf localhost:7777/readyz
```

The agent CLIs (`claude`, `codex`, `pi`, `opencode`) must be installed **and
logged in for the `kevin` user** — Kevin never holds provider credentials. Check
with `sudo -u kevin kevin workers doctor`.

## What the unit does and why

| Directive | Why |
|---|---|
| `ExecStartPre=kevin db migrate` | The `server` profile keeps `database.auto_migrate = false`, so exactly one process migrates and the daemon never starts against a schema it does not have. With pending migrations and no pre-start step, `kevin serve` stays up and `/readyz` reports `migrations_pending` instead of crash-looping. |
| `ExecReload=kill -HUP` | Re-reads `server.auth_token_file`; nothing else is hot-reloadable. |
| `TimeoutStopSec=60` | Must exceed `kevin.shutdown_grace_period` (30 s) + `workers.kill_grace` (10 s), or systemd `SIGKILL`s Kevin mid-drain and the attempts come back as `runtime_restarted` on the next start. |
| `Restart=on-failure` | A clean drain exits 0 and stays stopped; a crash comes back. |
| `StateDirectory=kevin` | systemd creates `/var/lib/kevin` 0750 owned by `kevin`; it is the only writable path (`ProtectSystem=strict` + `ReadWritePaths`). |
| `ProtectHome`, `PrivateTmp`, `RestrictAddressFamilies`, `CapabilityBoundingSet=` … | Threat T2/T5 of [`plan/09-security.md`](../../plan/09-security.md): a compromised worker subprocess inherits this sandbox. |
| `WorkingDirectory=/var/lib/kevin` | The daemon derives task workspaces from its working directory, so point it at the repository this instance works on (and add it to `ReadWritePaths`) when you want `workspace.strategy = "git_worktree"` to produce worktrees there. One daemon serves one repository root in v1. |

Verify the sandbox after any edit:

```bash
systemd-analyze security kevin.service      # aim for "OK"/"MEDIUM", not "UNSAFE"
```

> **Worker sandboxing.** `SystemCallFilter=@system-service` and
> `RestrictNamespaces=true` are inherited by every agent CLI Kevin spawns. If a
> worker needs containers (`sandbox.tier = "container"`), relax those two in a
> drop-in — do not weaken the rest.

## Reverse proxy

Keep `server.bind` on loopback and terminate TLS in front. SSE needs buffering
off, or the run stream arrives in chunks minutes late:

```nginx
location / {
    proxy_pass         http://127.0.0.1:7777;
    proxy_http_version 1.1;
    proxy_buffering    off;      # required for /api/v1/events
    proxy_read_timeout 1h;       # SSE streams are long-lived
    proxy_set_header   Host $host;
}
```

Caddy: `reverse_proxy 127.0.0.1:7777 { flush_interval -1 }`.

`/healthz` and `/readyz` are unauthenticated by design — expose them to your
load balancer, not to the internet. `/metrics` is **not** on the API port: set
`telemetry.metrics_bind` (e.g. `127.0.0.1:9464`) and scrape it there.

## Token rotation (no downtime)

```bash
sudo -u kevin kevin config rotate-token     # writes a new token, mode 0600
sudo systemctl reload kevin                 # SIGHUP → the daemon re-reads it
# old token keeps working for server.token_grace (5m): update clients meanwhile
```

Both tokens verify during the grace window; after it, the old one gets `401`.
Watch `kevin.api.auth_failed` in the journal to catch a client you forgot.

## Upgrade

```bash
curl -sf -XPOST -H "Authorization: Bearer $TOKEN" localhost:7777/api/v1/maintenance/drain
# /readyz now 503; wait for `running_attempts` to reach 0
sudo systemctl stop kevin
sudo install -m 0755 kevin-new /usr/local/bin/kevin
sudo -u kevin kevin db status && sudo -u kevin kevin db migrate
sudo systemctl start kevin && curl -sf localhost:7777/readyz
```

Restarting without draining is safe but not free: attempts that were running
are terminalised as `runtime_restarted` on the next startup and the run resumes
from its last satisfied saga step.

## Journal and metrics

Logs are JSON on stdout, one object per line — `journalctl -u kevin -o cat |
jq`. Useful filters: `select(.event | startswith("kevin.run"))`,
`select(.level == "ERROR")`. `RUST_LOG=debug` in `/etc/kevin/kevin.env` plus
`systemctl restart kevin` raises the level for a diagnosis session.

Runbooks for the symptoms this unit can produce (stuck run, projection lag, db
down, token compromised) are in
[`plan/10-observability-ops.md`](../../plan/10-observability-ops.md) §Runbooks.
