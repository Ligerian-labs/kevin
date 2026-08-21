# Changelog

All notable changes to Kevin are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The whole cargo workspace shares one version (`workspace.package.version`), so a
release covers every `kevin-*` crate and the `kevin` binary at once. Cutting a
release is described in [docs/releasing.md](docs/releasing.md).

Entries are grouped by the workstream that produced them
([plan/12-workstreams.md](plan/12-workstreams.md)) because, until 0.1.0, the
history is a build-out rather than a series of user-visible changes.

## [Unreleased]

Pre-1.0 build-out towards the first end-to-end release. Kevin is not yet a
released runtime: several CLI commands are still scaffolding, and no version has
been tagged, so nothing below has ever shipped.

### Added

- **Bootstrap (WS-00).** Cargo workspace with the `kevin-*` crate map from
  `plan/01-architecture.md`, the shared vocabulary in `kevin-domain` (id and
  kind newtypes, `EventEnvelope`, `Clock`/`IdGen`), the `kevin` CLI skeleton
  with one module per subcommand, the `just ci` gate (fmt, Clippy with warnings
  denied, `cargo-deny`, nextest) and the GitHub Actions CI workflow with a
  Postgres 16 + pgvector service.
- **Domain model (WS-01).** Event-sourced `Run`, `Task`, `Question`,
  `Evaluation`, `RouteScore` and `MemoryItem` aggregates with their state
  machines, the domain event catalog with `schema_version`, `Budget`/`Usage`
  arithmetic, the plan validator, and given/when/then test helpers in
  `kevin-testkit`.
- **Configuration (WS-02).** The full TOML schema from
  `plan/03-config-schema.md`, layered loading (defaults → user file → project
  file → `KEVIN__*` environment → CLI flags), aggregated validation errors,
  profile defaults, and `kevin config show|validate|init`.
- **Event store and Postgres platform (WS-03).** `EventStore` with optimistic
  concurrency, the command log, checkpoints, snapshots, the transactional
  outbox and its relay, the embedded migrations runner, `kevin db` commands and
  the template-database test harness.
- **Telemetry and event bus (WS-04).** `kevin-telemetry` (JSON logs, span
  fields, `kevin_` metrics, secret redaction) and `kevin-bus` with an in-process
  broadcast bus plus a Postgres `LISTEN`/`NOTIFY` bus that catches up from the
  store and reports lag instead of dropping events.
- **Worker core (WS-05).** The `Worker` trait, the subprocess supervisor
  (process groups, kill grace, bounded output streams), the worker registry and
  `doctor`, structured-output extraction and validation, and the scriptable
  fake worker used by every test that must not spend money.
- **Workspace isolation (WS-07).** Per-attempt git worktrees and jj workspaces,
  repo-kind detection, result integration (PR, merge, none), the environment
  allow-list and the sandbox policy with forbidden-flag checks.
- **Worker adapters.** `claude` (WS-06, `stream-json` mapping, JSON-schema
  output, permission-mode policy checks), `codex` (WS-13, `exec --json`
  mapping, resume, sandbox policy), `pi` (WS-14, JSON mode, prompt-instruction
  schema, provider auth checks) and `opencode` (WS-15, `--format json`, session
  repair turn, `--auto` policy).
- **Orchestration engine (WS-08).** The run saga and `RunActor`, the domain
  services behind it, the scheduler with its budget and concurrency bulkheads,
  and the task runner that turns a routed task into a supervised worker
  attempt.
- **Roles (WS-10).** Planner, judge and integrator prompts with their JSON
  schemas, response parsers and the `RoleRunner` that drives them through a
  worker.
- **Read models (WS-11).** Orchestration projections, the projection runner and
  `kevin db rebuild-projection`.
- **Routing (WS-09).** The model catalog and price table, persisted route
  scores and the Thompson-sampling router.
- **Memory (WS-18).** The pgvector-backed memory store, local embeddings,
  hybrid (vector + lexical) retrieval and the `kevin memory` commands.
- **Release engineering (WS-21).** `kevin --version` now reports the semver,
  the abbreviated commit id and the build date; the `release` workflow builds
  the four supported binary targets with checksums and a multi-arch container
  image with SBOM, provenance and a cosign signature; `deploy/Dockerfile` and
  `deploy/README.md` describe the daemon image; this changelog and
  `docs/releasing.md` describe how a release is cut.
- **Documentation.** The implementation plan in `plan/` — the authoritative
  specification for vision, architecture, domain model, config schema, workers,
  orchestration, memory and learning, the Kohral runtime contract, security,
  observability and operations, testing strategy, workstreams and roadmap —
  plus the project overview in `README.md`.

Not in this build, so you know what you are not getting: there is no
daemon entry point yet (`kevin serve`, `kevin run` and the TUI are scaffolding
pending WS-12, WS-16, WS-17 and WS-20), the Kohral runtime contract (WS-22) and
Kohral image (WS-23) are not implemented, and nothing is published to crates.io
(every crate is `publish = false`).

[Unreleased]: https://github.com/Ligerian-labs/kevin/commits/main
