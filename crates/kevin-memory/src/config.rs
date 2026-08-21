//! The resolved `[memory]` configuration (`plan/03-config-schema.md`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_config::{Embedder as ConfigEmbedder, KevinConfig, Memory};

/// Which embedding backend to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EmbedderKind {
    /// Local ONNX embeddings through fastembed.
    Fastembed,
    /// No embeddings: items are stored with `embedding NULL` and search
    /// degrades to full text + importance.
    None,
}

impl EmbedderKind {
    /// Config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            EmbedderKind::Fastembed => "fastembed",
            EmbedderKind::None => "none",
        }
    }
}

impl From<ConfigEmbedder> for EmbedderKind {
    fn from(value: ConfigEmbedder) -> Self {
        match value {
            ConfigEmbedder::Fastembed => EmbedderKind::Fastembed,
            ConfigEmbedder::None => EmbedderKind::None,
        }
    }
}

/// `[memory]` as this crate needs it, with `data_dir` and the blocking-pool
/// bound already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCfg {
    /// Memory enabled at all.
    pub enabled: bool,
    /// Which embedder to build.
    pub embedder: EmbedderKind,
    /// Embedding model name (`BAAI/bge-small-en-v1.5`).
    pub embedding_model: String,
    /// Vector width; must equal the `vector(N)` column.
    pub dimensions: usize,
    /// Items returned per query.
    pub top_k: usize,
    /// Cosine floor below which a hit needs a lexical match to survive.
    pub min_similarity: f32,
    /// Cap of the rendered `<kevin-memory>` block, in (estimated) tokens.
    pub context_max_tokens: usize,
    /// Store run summaries automatically.
    pub store_run_summaries: bool,
    /// Store artifact summaries automatically.
    pub store_artifact_summaries: bool,
    /// Importance decay half-life used for ranking (never for deletion).
    pub decay_half_life_days: f32,
    /// Where model files are cached (`<data_dir>/embeddings`).
    pub model_cache_dir: PathBuf,
    /// Upper bound on concurrent blocking embedding calls.
    pub blocking_threads: usize,
    /// How long to wait for the model to load before giving up.
    pub load_timeout: Duration,
}

impl Default for MemoryCfg {
    fn default() -> Self {
        Self::from_memory(&Memory::default(), Path::new("."), 4)
    }
}

impl MemoryCfg {
    /// Resolves `[memory]`, `[kevin].data_dir` and `[concurrency].blocking_threads`.
    #[must_use]
    pub fn from_config(config: &KevinConfig) -> Self {
        Self::from_memory(
            &config.memory,
            &config.kevin.data_dir,
            config.concurrency.blocking_threads as usize,
        )
    }

    /// Resolves a `[memory]` section against an explicit data directory.
    ///
    /// The casts are safe by construction: `min_similarity` is a 0..1 ratio and
    /// `decay_half_life_days` a small day count (both validated by
    /// `kevin-config`), and ranking works in `f32` throughout.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn from_memory(memory: &Memory, data_dir: &Path, blocking_threads: usize) -> Self {
        Self {
            enabled: memory.enabled,
            embedder: memory.embedder.into(),
            embedding_model: memory.embedding_model.clone(),
            dimensions: memory.dimensions as usize,
            top_k: memory.top_k as usize,
            min_similarity: memory.min_similarity as f32,
            context_max_tokens: memory.context_max_tokens as usize,
            store_run_summaries: memory.store_run_summaries,
            store_artifact_summaries: memory.store_artifact_summaries,
            decay_half_life_days: memory.decay_half_life_days.max(1) as f32,
            model_cache_dir: data_dir.join("embeddings"),
            blocking_threads: blocking_threads.max(1),
            load_timeout: Duration::from_secs(600),
        }
    }

    /// Test/CLI helper: the same configuration with another embedder.
    #[must_use]
    pub fn with_embedder(mut self, embedder: EmbedderKind, model: impl Into<String>) -> Self {
        self.embedder = embedder;
        self.embedding_model = model.into();
        self
    }

    /// Test helper: the same configuration with another vector width.
    #[must_use]
    pub const fn with_dimensions(mut self, dimensions: usize) -> Self {
        self.dimensions = dimensions;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_the_config_schema() {
        let cfg = MemoryCfg::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.embedder, EmbedderKind::Fastembed);
        assert_eq!(cfg.dimensions, 384);
        assert_eq!(cfg.top_k, 8);
        assert_eq!(cfg.context_max_tokens, 2500);
        assert!((cfg.decay_half_life_days - 90.0).abs() < f32::EPSILON);
        assert!(cfg.model_cache_dir.ends_with("embeddings"));
    }
}
