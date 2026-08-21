//! Memory core context (`plan/06-memory-and-learning.md` §Memory).
//!
//! Owns memory items (lessons, preferences, facts, run/artifact summaries),
//! the [`Embedder`] trait (local fastembed by default), the pgvector-backed
//! [`MemoryStore`] with hybrid retrieval and decay, redaction before storage
//! and the [`ContextBuilder`] used by the planner. Schema `memory.*`
//! (migration `crates/kevin-store/migrations/0004_memory.sql`).
//!
//! Module map:
//! - [`item`] — `MemoryKind`, `MemoryItem`, `MemorySource`, `Scope`/`RepoId`.
//! - [`embed`] — [`Embedder`], [`NoopEmbedder`], [`FixedEmbedder`] (+
//!   [`FastEmbedEmbedder`] behind the `fastembed` feature).
//! - [`rank`] — the hybrid scoring formula (pure, unit-tested).
//! - [`store`] — [`MemoryStore`]: store, supersede, forget, search, reindex.
//! - [`context`] — [`ContextBuilder`] rendering the `<kevin-memory>` block.
//! - [`summarize`] — the [`Summarizer`] contract and the deterministic
//!   [`ExtractiveSummarizer`].
//! - [`events`] — `memory.*` event names and payloads.
//!
//! Dependency direction: depends on `kevin-domain`, `kevin-config`,
//! `kevin-store` and `kevin-telemetry`. It deliberately does **not** depend on
//! `kevin-worker`: the worker-backed summariser is wired by the orchestrator
//! against the [`Summarizer`] trait defined here.
//!
//! ```no_run
//! use std::sync::Arc;
//! use kevin_memory::{MemoryStore, MemoryCfg, NoopEmbedder, SearchQuery, StoreRequest};
//!
//! # async fn demo(pool: sqlx::PgPool) -> Result<(), kevin_memory::MemoryError> {
//! let cfg = MemoryCfg::default();
//! let store = MemoryStore::new(pool, Arc::new(NoopEmbedder::new(cfg.dimensions)), cfg);
//! store.store(StoreRequest::fact("Kevin runs `just ci` before every PR")).await?;
//! let hits = store.search(SearchQuery::new("just ci")).await?;
//! # let _ = hits; Ok(()) }
//! ```

pub mod config;
pub mod context;
pub mod embed;
pub mod error;
pub mod events;
pub mod item;
pub mod rank;
pub mod store;
pub mod summarize;

pub use config::{EmbedderKind, MemoryCfg};
pub use context::{ContextBlock, ContextBuilder};
#[cfg(feature = "fastembed")]
pub use embed::fastembed_backend::FastEmbedEmbedder;
pub use embed::{
    EmbedError, Embedder, FixedEmbedder, MAX_BATCH, MAX_INPUT_CHARS, NoopEmbedder,
    embedder_from_cfg,
};
pub use error::{MemoryError, Result};
pub use item::{
    INTAKE_KINDS, MemoryKind, MemoryRecord, MemoryScope, MemorySource, RepoId, ScopeFilter,
    TASK_KINDS, parse_kind,
};
pub use rank::{W_IMPORTANCE, W_LEXICAL, W_SIMILARITY, decay, hybrid_score};
pub use store::{
    ExportItem, ForgetFilter, Hit, ImportReport, Lesson, MemoryCounts, MemoryStore, ReindexReport,
    SearchQuery, StoreRequest,
};
pub use summarize::{
    ArtifactInput, ArtifactSummary, ExtractiveSummarizer, MIN_PREFERENCE_CONFIDENCE, Preference,
    PreferenceScope, SUMMARIZER_SYSTEM_PROMPT, Summarizer, SummaryOutput, SummaryRequest,
    summary_json_schema,
};
