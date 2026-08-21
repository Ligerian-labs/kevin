//! Workers supporting context (`plan/04-workers.md`).
//!
//! A worker drives one external coding-agent CLI (`claude`, `codex`, `pi`,
//! `opencode`) or the in-process [`fake::FakeWorker`] to execute one task
//! attempt. Workers know nothing about runs, routing or evaluation: they
//! receive a [`TaskAttemptRequest`], spawn a process in the attempt's
//! workspace, normalise its output into [`WorkerEvent`]s and finish with a
//! [`WorkerOutcome`].
//!
//! Module map (frozen names, extend rather than rename):
//! - [`claude`] — the `claude` (Claude Code) CLI adapter.
//! - [`codex`] — the `codex` (OpenAI Codex) CLI adapter.
//! - [`types`] — request-side value objects (`TaskAttemptRequest`, `Usage`, …).
//! - [`worker`] — the [`Worker`] trait, events, outcomes, handle, doctor.
//! - [`supervisor`] — subprocess supervision: process groups, kill grace,
//!   bounded line streams, transcripts, exit classification.
//! - [`registry`] — [`WorkerRegistry`] built from configuration.
//! - [`fake`] — scenario-driven in-process worker (tests, Kohral conformance).
//! - [`structured`] — structured-output extraction and JSON-schema validation.
//! - [`usage`] — usage normalisation and the [`usage::PriceTable`] cost hook.
//! - [`policy`] — minimal sandbox policy consumed by adapters
//!   (`// TODO(ws-07)`: replaced by `kevin_workspace::SandboxPolicy`).
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`. Implemented by WS-05 (core + fake) and WS-06/13/14/15
//! (adapters).

pub mod claude;
pub mod codex;
pub mod fake;
pub mod policy;
pub mod registry;
pub mod structured;
pub mod supervisor;
pub mod types;
pub mod usage;
pub mod worker;

pub use claude::ClaudeWorker;
pub use codex::CodexWorker;
pub use policy::{SandboxPolicy, SandboxTier};
pub use registry::{RegistryConfig, WorkerCfg, WorkerRegistry};
pub use supervisor::{ChildExit, ChildHandle, ExitReason, SpawnOpts, Supervisor, Verdict};
pub use types::{
    ArtifactKind, ArtifactRef, AttemptBudget, AttemptContext, ConfigError, ConfigErrors,
    EnvAllowlist, ModelEntry, Route, TaskAttemptRequest, TaskSpec, Usage, Workspace, WorkspaceKind,
    WorkspacePolicy,
};
pub use worker::{
    AuthStatus, Doctor, EventSink, Worker, WorkerError, WorkerEvent, WorkerHandle, WorkerOutcome,
    WorkerSessionId,
};
