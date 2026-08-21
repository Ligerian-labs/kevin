# Kevin — agent instructions
Kevin: Rust, event-driven autonomous agent runtime orchestrating coding-agent CLIs (claude/codex/pi/opencode).
Stack: tokio, Postgres 16 + pgvector (sqlx), axum API, ratatui TUI, clap CLI; modular monolith, event-sourced.
Repo: cargo workspace `crates/kevin-*` (one bounded context per crate, dependency direction in `plan/01`),
`plan/` is the authoritative spec (read `plan/README.md`, `00`–`03`, then your entry in `plan/12-workstreams.md`),
`deploy/compose/postgres.yml` local Postgres, `testing/` workspace-level test assets.
Commands: `just ci` (fmt --check, clippy -D warnings, cargo-deny, nextest) must be green before any PR;
`just db-up` / `just db-down` start/stop pgvector; `just test -p <crate>` for one crate.
Tests need `DATABASE_URL` (default `postgres://kevin:kevin@localhost:5433/kevin`); each test makes its own database.
Workflow: use jj (never work on main; `ws switch ws-NN-<slug>`), PRs to `main`, conventional commits
(`feat(ws-NN): …`), no AI attribution trailers, acceptance tests named `ac_wsNN_<n>_<slug>` written first.
Frozen interfaces (plan/12 "Provides") change only via a plan PR; `CLAUDE.md`/`AGENTS.md` stay ≤ 15 lines.
