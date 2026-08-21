# Architecture decision records

Short, numbered, immutable once accepted. Supersede with a new ADR rather than editing.

| # | Title | Status |
|---|-------|--------|
| [0001](./0001-rust-tokio-process-model.md) | Rust, tokio multi-thread runtime; agents are supervised tasks | accepted |
| [0002](./0002-postgres-event-sourcing.md) | Postgres + pgvector as the only store; event-sourced aggregates + projections | accepted |
| [0003](./0003-workers-are-external-clis.md) | Workers are external coding-agent CLIs behind a `Worker` trait | accepted |
| [0004](./0004-local-embeddings.md) | Local embeddings (fastembed) by default | accepted |
| [0005](./0005-toml-layered-config.md) | TOML configuration with layered precedence and strict validation | accepted |
| [0006](./0006-axum-ratatui-interfaces.md) | axum API + SSE; ratatui TUI as a pure API client | accepted |
| [0007](./0007-workspace-isolation.md) | One git worktree / jj workspace per task attempt | accepted |
| [0008](./0008-kohral-hermes-contract.md) | Kohral compatibility via the Hermes-style runtime contract in an ACL crate | accepted |
| [0009](./0009-wasm-deferred-sandbox-tiers.md) | WASM deferred; sandbox tiers now | accepted |
| [0010](./0010-evaluation-auto-apply-policy.md) | Evaluations auto-apply only to routing scores and memory | accepted |
