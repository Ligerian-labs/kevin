//! Local ONNX embeddings through the `fastembed` crate (`plan/06` §1.2,
//! ADR 0004). Enabled by the crate's default `fastembed` feature.
//!
//! The model (`BAAI/bge-small-en-v1.5`, 384 dims) is downloaded once (~130 MB,
//! logged at info) into `<data_dir>/embeddings` and loaded from that cache
//! afterwards — no network at runtime once the cache is warm, which is what
//! `kevin memory doctor` pre-fetches and what the Kohral image pre-bakes.
//!
//! Inference is CPU-bound: every call runs on `tokio::task::spawn_blocking`
//! and is bounded by a semaphore of `concurrency.blocking_threads` permits.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tokio::sync::{Mutex, Semaphore};

use crate::config::MemoryCfg;
use crate::embed::{EmbedError, Embedder, MAX_BATCH, truncate_input};

/// Local embedder backed by fastembed/ONNX Runtime.
pub struct FastEmbedEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    permits: Arc<Semaphore>,
    model_name: String,
    dimensions: usize,
    cache_dir: PathBuf,
}

impl fmt::Debug for FastEmbedEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FastEmbedEmbedder")
            .field("model_name", &self.model_name)
            .field("dimensions", &self.dimensions)
            .field("cache_dir", &self.cache_dir)
            .field("permits", &self.permits.available_permits())
            .finish_non_exhaustive()
    }
}

impl FastEmbedEmbedder {
    /// Loads (downloading on first use) the model named by `cfg.embedding_model`
    /// into `cfg.model_cache_dir`, and checks that its width matches
    /// `cfg.dimensions`.
    pub async fn load(cfg: &MemoryCfg) -> Result<Self, EmbedError> {
        let (model, dimensions) = resolve_model(&cfg.embedding_model)?;
        if dimensions != cfg.dimensions {
            return Err(EmbedError::Dimensions {
                expected: cfg.dimensions,
                actual: dimensions,
            });
        }
        let cache_dir = cfg.model_cache_dir.clone();
        std::fs::create_dir_all(&cache_dir).map_err(|e| EmbedError::Load {
            model: cfg.embedding_model.clone(),
            message: format!("cannot create the model cache {}: {e}", cache_dir.display()),
        })?;
        let warm = is_cached(&cache_dir);
        if !warm {
            tracing::info!(
                model = %cfg.embedding_model,
                cache = %cache_dir.display(),
                "downloading the embedding model (~130 MB, once)"
            );
        }
        let name = cfg.embedding_model.clone();
        let cache = cache_dir.clone();
        let loaded = tokio::task::spawn_blocking(move || {
            TextEmbedding::try_new(
                InitOptions::new(model)
                    .with_cache_dir(cache)
                    .with_show_download_progress(false),
            )
        })
        .await
        .map_err(|_| EmbedError::Cancelled)?
        .map_err(|e| EmbedError::Load {
            model: name.clone(),
            message: e.to_string(),
        })?;
        tracing::info!(model = %name, dimensions, cache = %cache_dir.display(), "embedding model ready");
        Ok(Self {
            model: Arc::new(Mutex::new(loaded)),
            permits: Arc::new(Semaphore::new(cfg.blocking_threads)),
            model_name: name,
            dimensions,
            cache_dir,
        })
    }

    /// Where model files are cached.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Whether `cache_dir` already holds model files (nothing to download).
    #[must_use]
    pub fn is_cache_warm(&self) -> bool {
        is_cached(&self.cache_dir)
    }
}

#[async_trait]
impl Embedder for FastEmbedEmbedder {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let texts: Vec<String> = inputs.iter().map(|t| truncate_input(t)).collect();
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| EmbedError::Cancelled)?;
        let model = Arc::clone(&self.model);
        let expected = self.dimensions;
        let vectors = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            guard.embed(texts, Some(MAX_BATCH))
        })
        .await
        .map_err(|_| EmbedError::Cancelled)?
        .map_err(|e| EmbedError::Backend(e.to_string()))?;
        if let Some(actual) = vectors.iter().map(Vec::len).find(|len| *len != expected) {
            return Err(EmbedError::Dimensions { expected, actual });
        }
        Ok(vectors)
    }
}

/// Maps a configured model name (`BAAI/bge-small-en-v1.5`) to a fastembed
/// model and its width. fastembed publishes the same weights under mirror
/// namespaces (`Xenova/…`, `Qdrant/…`), so the repository *name* is matched,
/// not the namespace.
pub fn resolve_model(name: &str) -> Result<(EmbeddingModel, usize), EmbedError> {
    let wanted = name.trim().to_lowercase();
    let basename = wanted.rsplit('/').next().unwrap_or(&wanted).to_owned();
    let models = TextEmbedding::list_supported_models();
    let found = models
        .iter()
        .find(|info| info.model_code.to_lowercase() == wanted)
        .or_else(|| {
            models.iter().find(|info| {
                info.model_code
                    .to_lowercase()
                    .rsplit('/')
                    .next()
                    .is_some_and(|code| code == basename)
            })
        });
    match found {
        Some(info) => Ok((info.model.clone(), info.dim)),
        None => Err(EmbedError::Load {
            model: name.to_owned(),
            message: format!(
                "unknown fastembed model; known models: {}",
                models
                    .iter()
                    .map(|i| i.model_code.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

/// Whether any `*.onnx` file is present under `dir` (cheap "is it downloaded").
fn is_cached(dir: &Path) -> bool {
    fn walk(dir: &Path, depth: usize) -> bool {
        if depth > 4 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if walk(&path, depth + 1) {
                    return true;
                }
            } else if path.extension().is_some_and(|e| e == "onnx") {
                return true;
            }
        }
        false
    }
    walk(dir, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_model_resolves_to_384_dimensions() {
        let (_, dims) = resolve_model("BAAI/bge-small-en-v1.5").expect("known model");
        assert_eq!(dims, 384);
        let (_, same) = resolve_model("bge-small-en-v1.5").expect("bare name works too");
        assert_eq!(same, 384);
        assert!(resolve_model("acme/not-a-model").is_err());
    }

    #[test]
    fn an_empty_cache_directory_is_cold() {
        let dir = std::env::temp_dir().join(format!("kevin-fe-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        assert!(!is_cached(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
