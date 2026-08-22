# Kevin migrations

All SQL migrations of the whole workspace live here and are embedded into
`kevin-store` with `sqlx::migrate!()` (`kevin_store::MIGRATOR`). They are applied
by `kevin db migrate` / `kevin_store::migrate(pool, policy)` and by the test
harness (`kevin_testkit::pg::TestDb`, which migrates a template database once
per migration set).

## Numbering scheme (shared file, one schema per workstream)

- File name: `NNNN_<schema>_<what>.sql`, where `NNNN` is a zero-padded, strictly
  increasing 4-digit number (sqlx orders by this integer — the rest of the file
  name is a description). The number is the migration version recorded in
  `public._sqlx_migrations`.
- Each workstream owns one schema (see `plan/01-architecture.md` §Storage) and
  creates it in its own file: `core` (WS-03, `0001_core.sql`), `orch` (WS-11),
  `routing` (WS-09), `memory` (WS-18), `eval` (WS-19), `kohral` (WS-22).
  Reserve your number by taking the next free one when you open your PR; a
  clash with a concurrently merged PR is resolved by renumbering the later PR
  (never an already-merged file).
- Migrations are **additive only** (`plan/10-observability-ops.md` §Migrations):
  expand → backfill → switch → contract across releases; never rename or drop a
  column in the same release that stops writing it. Never edit an applied file
  (the checksum is verified at startup; `kevin db status` reports mismatches).
- Use `IF NOT EXISTS` for schemas/tables/indexes so a file stays safe to re-run
  by hand, and keep every file runnable inside one transaction (no
  `CREATE INDEX CONCURRENTLY`; sqlx wraps each migration in a transaction).
- `CREATE EXTENSION IF NOT EXISTS vector` lives **only** in `0001_core.sql`.
  Later schemas that need `vector(N)` columns rely on it being present.
- Event payload evolution never rewrites `core.events`: bump `schema_version`
  and register an upcaster (`kevin_store::upcast::Upcasters`).

Current allocation:

| Version | File | Schema | Owner |
|---|---|---|---|
| 0001 | `0001_core.sql` | `core` (+ `vector` extension) | WS-03 |
| 0002 | `0002_orch.sql` | `orch` (projections / read models) | WS-11 |
| 0003 | `0003_routing.sql` | `routing` | WS-09 |
| 0004 | `0004_memory.sql` | `memory` | WS-18 |
| 0005 | `0005_eval.sql` | `eval` (evaluations, proposals inbox) | WS-19 |
| 0006 | `0006_kohral.sql` | `kohral` (runs ledger, session messages) | WS-22 |
