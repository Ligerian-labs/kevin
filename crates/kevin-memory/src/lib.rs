//! Memory core context (`plan/06-memory-and-learning.md` §Memory).
//!
//! Owns memory items (lessons, preferences, facts, run/artifact summaries),
//! the `Embedder` trait (local fastembed by default), the pgvector-backed
//! `MemoryStore` with hybrid retrieval and decay, redaction before storage and
//! the context builder used by the planner. Schema `memory.*`.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-telemetry` (and `kevin-store`, `kevin-worker` for summarisation,
//! added by WS-18). Implemented by WS-18.
