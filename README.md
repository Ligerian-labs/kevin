# Kevin

> “Everybody needs to have a Kevin in his life.”

Kevin is an autonomous agent runtime for the coding agents you already use. It
turns a goal into a plan, delegates the work to `claude`, `codex`, `pi`, or
`opencode`, integrates their results, and evaluates the outcome so future runs
can make better decisions.

Run Kevin on a laptop, operate it as a daemon on a VPS, or deploy it as a
[Kohral](https://github.com/Ligerian-labs/kohral) runtime. WASM-hosted tools and
agents are part of the post-v1 roadmap, not the current runtime.

## Why Kevin?

The name comes from a simple idea: Kevin is common enough to feel universal,
and no single trade defines a Kevin. A Kevin might work in finance, write code,
or run a farm. This Kevin is built in the same spirit—a general-purpose
orchestrator that finds the right specialist for the job and helps it succeed.

## What Kevin does

Kevin is a **meta-agent**, not another coding-agent loop. It provides the
coordination, persistence, isolation, and learning layer around existing agent
CLIs:

- **Understands before acting.** A frontier model restates the objective,
  identifies risks and assumptions, and asks focused questions where human
  intent is genuinely ambiguous.
- **Plans and delegates.** Kevin turns the refined goal into a dependency-aware
  task graph and can execute independent tasks concurrently.
- **Routes work to the right model.** Configured model aliases and learned route
  scores let Kevin select a worker and model for each kind of task.
- **Isolates execution.** Every task attempt receives its own git worktree or
  jj workspace, a bounded budget, a timeout, and a supervised subprocess.
- **Remembers useful context.** Postgres and pgvector store lessons,
  preferences, summaries, and artifact context for later runs.
- **Evaluates every outcome.** Judge results update memory and routing scores;
  prompt and configuration changes remain proposals for a human to approve.
- **Keeps an audit trail.** Commands produce durable domain events, while
  projections power the CLI, API, event streams, and terminal UI.

```mermaid
flowchart LR
    G[User goal] --> U[Understand]
    U --> Q{Anything ambiguous?}
    Q -->|Yes| C[Clarify with the user]
    C --> P[Plan a task graph]
    Q -->|No| P
    P --> R[Route each task]
    R --> W[Run agents in isolated workspaces]
    W --> I[Integrate results]
    I --> E[Evaluate]
    E --> M[(Memory and route scores)]
    M -. improves .-> U
```

## Architecture

Kevin is a Rust modular monolith built on a multi-threaded Tokio runtime.
Agents run as supervised asynchronous tasks and invoke external coding-agent
CLIs as subprocesses. Postgres 16 with pgvector is the source of truth for the
event store, read models, routing data, evaluations, and memory.

The main interfaces are:

- a `kevin` CLI for local and remote runs;
- an axum HTTP API with durable SSE streams;
- a ratatui terminal UI;
- a Kohral compatibility layer for managed deployments.

The runtime is event-driven: aggregates validate commands and emit immutable
events; projections and downstream services react to those events. This keeps
long-running work recoverable, observable, and testable without coupling every
component to every other component.

## Security model

Coding agents execute tools and modify files, so Kevin treats execution as a
privileged operation. The v1 design combines each worker's native sandbox,
per-attempt workspace isolation, an environment allow-list, dangerous-flag
validation, cancellation trees, timeouts, and cost/token/concurrency budgets.

Kevin does not call model-provider HTTP APIs directly in v1. Authentication
stays with the installed worker CLIs. Container deployments may opt into a
stronger isolation tier; running with no sandbox is an explicit operator
choice.

See the [security model](plan/09-security.md) for the threat model and trust
boundaries.

## Project status

Kevin is under active development. The architecture and workstreams are
specified in detail, and foundational crates are being implemented in
parallel. The repository is not yet a released, end-to-end autonomous runtime;
some CLI commands are currently scaffolding for later milestones.

The [implementation plan](plan/README.md) is the authoritative product and
engineering specification. Useful entry points include:

- [vision and scope](plan/00-vision.md);
- [architecture](plan/01-architecture.md);
- [configuration and model routing](plan/03-config-schema.md);
- [parallel workstreams](plan/12-workstreams.md);
- [milestones and post-v1 roadmap](plan/13-roadmap.md).

## Installation

Kevin ships as a single `kevin` binary. It needs a Postgres 16 database with the
`pgvector` extension, and it invokes the coding-agent CLIs you already have
installed—it never bundles or authenticates them for you.

**From source.** The supported source install; requires the Rust toolchain in
`rust-toolchain.toml`.

```bash
cargo install --path crates/kevin-cli --locked
# without a checkout:
cargo install --git https://github.com/Ligerian-labs/kevin --locked kevin-cli
```

The crates are not published to crates.io, so `cargo install kevin-cli` does not
work. The reasoning is in [the release runbook](docs/releasing.md).

**Prebuilt binaries.** Every `vX.Y.Z` tag publishes stripped archives for
`x86_64`/`aarch64` Linux and Intel/Apple Silicon macOS, together with a
`SHA256SUMS` file.

```bash
tag=v0.1.0; target=aarch64-apple-darwin
base="https://github.com/Ligerian-labs/kevin/releases/download/$tag"
curl -fsSLO "$base/kevin-$target.tar.gz" && curl -fsSLO "$base/SHA256SUMS"
shasum -a 256 --ignore-missing -c SHA256SUMS   # sha256sum on Linux
tar xzf "kevin-$target.tar.gz" && install "kevin-$target/kevin" ~/.local/bin/
```

macOS binaries are not notarized; clear the quarantine attribute with
`xattr -d com.apple.quarantine kevin` after downloading.

**Container.** `ghcr.io/ligerian-labs/kevin` is the daemon image—multi-arch,
signed with cosign, published with an SBOM and provenance attestation. It runs
`kevin serve` and deliberately does not contain the agent CLIs.

```bash
podman run --rm -p 7777:7777 \
  -e KEVIN__DATABASE__URL="postgres://kevin:kevin@localhost:5433/kevin" \
  -v kevin-data:/var/lib/kevin \
  ghcr.io/ligerian-labs/kevin:latest
```

`kevin --version` prints the semver, the commit it was built from, and the build
date, so an installed binary always identifies itself. See
[deployment notes](deploy/README.md) for laptop, VPS, and container topologies,
and [the release runbook](docs/releasing.md) for signature verification.

## Development

The workspace targets Rust 1.94 and uses `just` for developer commands. The
full CI gate also requires `cargo-nextest` and `cargo-deny`. Postgres-backed
tests use the pgvector service in `deploy/compose/postgres.yml`, started through
Docker Compose or Podman Compose.

```bash
# Start the development database on localhost:5433.
just db-up

# Inspect the current CLI surface.
cargo run -p kevin-cli -- --help

# Run formatting checks, Clippy, cargo-deny, and all tests.
just ci

# Stop the development database without deleting its volume.
just db-down
```

Implementation is divided into independently owned workstreams. Before making
changes, read [the plan](plan/README.md) and the relevant entry in
[the workstream map](plan/12-workstreams.md). Frozen interfaces change only
through a plan update, and every workstream must keep `just ci` green.

## Runtime targets

| Target | Direction |
|---|---|
| Laptop | One binary with an embedded runtime and local API, backed by Postgres + pgvector. |
| VPS | Long-running daemon with remote CLI/TUI clients, health checks, metrics, and graceful drain. |
| Kohral | Native runtime adapter and container stack using the Hermes-style contract. |
| WASM | Post-v1 exploration for capability-scoped tools and, later, agent loops. |

