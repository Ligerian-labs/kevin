//! Embedders (`plan/06-memory-and-learning.md` §1.2, `plan/adr/0004-local-embeddings.md`).
//!
//! [`Embedder`] is the frozen trait; the default implementation is
//! [`fastembed_backend::FastEmbedEmbedder`] (local ONNX, `BAAI/bge-small-en-v1.5`,
//! 384 dims, model cached under `<data_dir>/embeddings`). [`NoopEmbedder`] is
//! the `embedder = "none"` path, and [`FixedEmbedder`] is the deterministic
//! embedder tests use — no model, no network, and similar texts still get
//! similar vectors (hashed bag of words).

#[cfg(feature = "fastembed")]
pub mod fastembed_backend;

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::{EmbedderKind, MemoryCfg};

/// Inputs longer than this are truncated before embedding (bge-small has a
/// 512-token window).
pub const MAX_INPUT_CHARS: usize = 2_000;

/// Maximum number of inputs per inference batch.
pub const MAX_BATCH: usize = 32;

/// Model name reported by [`NoopEmbedder`].
pub const NOOP_MODEL: &str = "none";

/// Model name reported by [`FixedEmbedder`].
pub const FIXED_MODEL: &str = "fixed-hash-v1";

/// Why embedding failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EmbedError {
    /// The configured embedder is `none`: callers store `embedding NULL`.
    #[error("embeddings are disabled (`memory.embedder = \"none\"`)")]
    Disabled,

    /// The backend returned vectors of an unexpected width.
    #[error("embedder returned {actual}-dimensional vectors, expected {expected}")]
    Dimensions {
        /// Expected width.
        expected: usize,
        /// Width actually returned.
        actual: usize,
    },

    /// Loading the model failed (download, cache, ONNX runtime…).
    #[error("cannot load embedding model `{model}`: {message}")]
    Load {
        /// Model name.
        model: String,
        /// Backend message.
        message: String,
    },

    /// Inference failed.
    #[error("embedding failed: {0}")]
    Backend(String),

    /// The blocking task was cancelled (shutdown).
    #[error("embedding task was cancelled")]
    Cancelled,
}

/// Turns text into vectors. Frozen interface (`plan/12` WS-18 *Provides*).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Model name stored in `memory_items.embedding_model`.
    fn model_name(&self) -> &str;

    /// Vector width produced by this embedder.
    fn dimensions(&self) -> usize;

    /// One vector per input, in the same order. Inputs are pre-truncated to
    /// [`MAX_INPUT_CHARS`] by the caller.
    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Embeds one text (convenience over [`Embedder::embed_batch`]).
pub async fn embed_one(embedder: &dyn Embedder, input: &str) -> Result<Vec<f32>, EmbedError> {
    let inputs = vec![truncate_input(input)];
    let mut vectors = embedder.embed_batch(&inputs).await?;
    vectors.pop().ok_or(EmbedError::Backend(
        "embedder returned no vector for one input".to_owned(),
    ))
}

/// Truncates one input to [`MAX_INPUT_CHARS`] characters (on a char boundary).
#[must_use]
pub fn truncate_input(input: &str) -> String {
    if input.chars().count() <= MAX_INPUT_CHARS {
        return input.to_owned();
    }
    input.chars().take(MAX_INPUT_CHARS).collect()
}

/// Builds the embedder described by `cfg`.
///
/// `fastembed` loads (and, the first time, downloads ~130 MB into
/// `cfg.model_cache_dir`) the ONNX model; without the `fastembed` feature the
/// same configuration falls back to [`NoopEmbedder`] with a warning, so a
/// binary built without the feature still runs.
pub async fn embedder_from_cfg(cfg: &MemoryCfg) -> Result<Arc<dyn Embedder>, EmbedError> {
    match cfg.embedder {
        EmbedderKind::None => Ok(Arc::new(NoopEmbedder::new(cfg.dimensions))),
        EmbedderKind::Fastembed => {
            #[cfg(feature = "fastembed")]
            {
                let embedder = fastembed_backend::FastEmbedEmbedder::load(cfg).await?;
                Ok(Arc::new(embedder))
            }
            #[cfg(not(feature = "fastembed"))]
            {
                tracing::warn!(
                    model = %cfg.embedding_model,
                    "memory.embedder = \"fastembed\" but this binary was built without the \
                     `fastembed` feature; falling back to no embeddings (lexical search only)"
                );
                Ok(Arc::new(NoopEmbedder::new(cfg.dimensions)))
            }
        }
    }
}

/// `embedder = "none"`: stores items without a vector; search degrades to
/// full-text + importance.
#[derive(Debug, Clone, Copy)]
pub struct NoopEmbedder {
    dimensions: usize,
}

impl NoopEmbedder {
    /// A no-op embedder reporting the configured width.
    #[must_use]
    pub const fn new(dimensions: usize) -> Self {
        Self { dimensions }
    }
}

impl Default for NoopEmbedder {
    fn default() -> Self {
        Self::new(384)
    }
}

#[async_trait]
impl Embedder for NoopEmbedder {
    fn model_name(&self) -> &str {
        NOOP_MODEL
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed_batch(&self, _inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Disabled)
    }
}

/// Deterministic, dependency-free embedder for tests and `--dry-run` tooling.
///
/// Hashed bag of words: every lowercase alphanumeric token is hashed into one
/// of `dimensions` buckets with a ±1 sign, the vector is L2-normalised. Two
/// texts sharing words are close in cosine space, the same text always maps to
/// the same vector, and no model or network is involved.
#[derive(Debug, Clone)]
pub struct FixedEmbedder {
    dimensions: usize,
    model_name: String,
}

impl FixedEmbedder {
    /// A fixed embedder of `dimensions` width, named [`FIXED_MODEL`].
    #[must_use]
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(1),
            model_name: FIXED_MODEL.to_owned(),
        }
    }

    /// Same, with an explicit model name (to test `reindex`, which selects
    /// rows by `embedding_model`).
    #[must_use]
    pub fn named(dimensions: usize, model_name: impl Into<String>) -> Self {
        Self {
            dimensions: dimensions.max(1),
            model_name: model_name.into(),
        }
    }

    /// The vector for `text` (pure function, useful in assertions).
    #[must_use]
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; self.dimensions];
        for token in tokens(text) {
            let hash = fnv1a(token.as_bytes());
            let bucket = usize::try_from(hash % self.dimensions as u64).unwrap_or(0);
            let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
            vector[bucket] += sign;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for v in &mut vector {
                *v /= norm;
            }
        } else {
            // Empty/stop-word-only text: a fixed unit vector keeps cosine defined.
            vector[0] = 1.0;
        }
        vector
    }
}

impl Default for FixedEmbedder {
    fn default() -> Self {
        Self::new(384)
    }
}

#[async_trait]
impl Embedder for FixedEmbedder {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(inputs.iter().map(|text| self.vector(text)).collect())
    }
}

fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[tokio::test]
    async fn noop_embedder_reports_disabled() {
        let embedder = NoopEmbedder::new(384);
        assert_eq!(embedder.dimensions(), 384);
        assert_eq!(embedder.model_name(), "none");
        assert_eq!(
            embedder.embed_batch(&["x".to_owned()]).await,
            Err(EmbedError::Disabled)
        );
    }

    #[tokio::test]
    async fn fixed_embedder_is_deterministic_and_semantic_enough() {
        let embedder = FixedEmbedder::new(64);
        let vectors = embedder
            .embed_batch(&[
                "run cargo fmt before opening a PR".to_owned(),
                "cargo fmt must run before every PR".to_owned(),
                "the cat sleeps on a warm roof".to_owned(),
            ])
            .await
            .unwrap();
        assert_eq!(vectors.len(), 3);
        assert!(vectors.iter().all(|v| v.len() == 64));
        assert_eq!(
            vectors[0],
            embedder.vector("run cargo fmt before opening a PR")
        );
        let related = cosine(&vectors[0], &vectors[1]);
        let unrelated = cosine(&vectors[0], &vectors[2]);
        assert!(
            related > unrelated,
            "shared words must score higher: {related} vs {unrelated}"
        );
        assert!((cosine(&vectors[0], &vectors[0]) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn inputs_are_truncated_to_the_model_window() {
        let long = "x".repeat(MAX_INPUT_CHARS * 2);
        assert_eq!(truncate_input(&long).chars().count(), MAX_INPUT_CHARS);
        assert_eq!(truncate_input("short"), "short");
    }
}
