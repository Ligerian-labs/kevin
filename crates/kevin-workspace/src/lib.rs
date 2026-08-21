//! Workspace & sandbox supporting context (`plan/09-security.md`, ADR 0007).
//!
//! Owns per-attempt workspace isolation (git worktree, jj workspace, in-place),
//! repository kind detection, the environment allow-list, the sandbox tier
//! policy (`cli-native` | `container` | `none`) with forbidden-flag checks, and
//! result integration (PR via `gh`, merge, none).
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`. Implemented by WS-07.
//!
//! Module map (frozen names from `plan/12-workstreams.md` §WS-07):
//! - [`repo`] — [`RepoKind::detect`].
//! - [`workspace`] — [`WorkspaceManager`] (`prepare` / `cleanup`).
//! - [`integrate`] — [`Integrator`] (`pr` | `merge` | `none`).
//! - [`env`] — [`EnvAllowlist::build`].
//! - [`sandbox`] — [`SandboxPolicy`], [`FORBIDDEN_FLAGS`], `check_argv`.
//! - [`cmd`] — the injectable [`CommandRunner`] every git/jj/gh call goes through.
//! - [`config`] — the `[workspace]`, `[sandbox]`, `[checks]` sections
//!   (re-exported from `kevin-config` under this crate's names).
//! - [`model`] — `Workspace`, `ArtifactRef` value objects (see `TODO(ws-01)`).
//!
//! Every external call (git, jj, gh, repo checks) goes through the
//! [`CommandRunner`] trait so tests can record and stub commands; all
//! operations are synchronous — callers on a tokio runtime wrap them in
//! `spawn_blocking`.

pub mod cmd;
pub mod config;
pub mod env;
pub mod integrate;
pub mod model;
pub mod repo;
pub mod sandbox;
pub mod workspace;

mod util;

pub use cmd::{Cmd, CmdError, CmdOutput, CommandRunner, ProcessRunner};
pub use config::{
    ChecksConfig, CleanupPolicy, IntegrationMode, KevinConfig, NetworkPolicy, SandboxConfig,
    SandboxTier, Strategy, WorkspaceConfig,
};
pub use env::{EnvAllowlist, EnvAllowlistSpec, KevinEnv};
pub use integrate::{
    Conflict, IntegrationConfig, IntegrationError, IntegrationResult, IntegrationRun, Integrator,
};
pub use model::{ArtifactKind, ArtifactRef, Workspace, WorkspaceKind, WorkspacePolicy};
pub use repo::RepoKind;
pub use sandbox::{
    FORBIDDEN_FLAGS, ForbiddenFlag, ForbiddenFlagShape, PolicyViolation, SandboxPolicy, check_argv,
};
pub use workspace::{
    CleanupOutcome, PrepareRequest, ResolvedStrategy, WorkspaceError, WorkspaceManager,
};
