# Kevin documentation

The authoritative product and engineering specification lives in
[`plan/`](../plan/README.md) — start at `plan/README.md` and follow the table
there. This directory holds the operational documents that outlive a
workstream.

| Document | For |
|---|---|
| [releasing.md](releasing.md) | Cutting `vX.Y.Z`: version policy, what CI does with the tag, verifying signatures and checksums, rolling back. |
| [../deploy/README.md](../deploy/README.md) | Running Kevin on a laptop, on a VPS, or as a container: required environment, volumes, ports, health endpoints. |
| [../CHANGELOG.md](../CHANGELOG.md) | What changed in each release. |

Operational runbooks for a *running* Kevin (stuck runs, budget exhaustion,
projection lag, database outage) are in
[`plan/10-observability-ops.md`](../plan/10-observability-ops.md); the security
model and threat model are in [`plan/09-security.md`](../plan/09-security.md).
