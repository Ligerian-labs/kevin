//! Workspace & sandbox supporting context (`plan/09-security.md`, ADR 0007).
//!
//! Owns per-attempt workspace isolation (git worktree, jj workspace, in-place),
//! repository kind detection, the environment allow-list, the sandbox tier
//! policy (`cli-native` | `container` | `none`) with forbidden-flag checks, and
//! result integration (PR via `gh`, merge, none).
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`. Implemented by WS-07.
