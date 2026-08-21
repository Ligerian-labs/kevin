//! Evaluation core context (`plan/06-memory-and-learning.md` §Evaluation).
//!
//! Owns rubrics, judge prompts and the judge runner (through `kevin-worker`),
//! evaluation records, score normalisation, lessons extraction and proposals.
//! Evaluations auto-update routing scores and memory only; prompt/config
//! changes are proposals for a human. Schema `eval.*`.
//!
//! Dependency direction: depends on `kevin-worker`, `kevin-router`,
//! `kevin-memory` (and the platform crates); downstream of orchestration by
//! events only. Implemented by WS-19.
