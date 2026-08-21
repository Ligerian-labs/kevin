//! Workers supporting context (`plan/04-workers.md`).
//!
//! Owns the `Worker` trait and its adapters (`claude`, `codex`, `pi`,
//! `opencode`, in-process `fake`), the subprocess supervisor (process groups,
//! kill grace, bounded line streams), JSONL stream normalisation into
//! `WorkerEvent`s, usage extraction, structured-output validation, the
//! `WorkerRegistry` and `doctor` checks.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry`. Implemented by WS-05 (core + fake) and WS-06/13/14/15
//! (adapters).
