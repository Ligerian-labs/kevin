//! WS-18 acceptance criterion (6): the fastembed model loads from its cache
//! and produces 384-dimensional vectors.
//!
//! The model is ~130 MB and the first load downloads it, so this test is
//! **opt-in**: it runs only with `KEVIN_FASTEMBED_TESTS=1` (CI caches
//! `<data_dir>/embeddings` and sets the variable; the default `just ci` run and
//! every other test in this crate use `FixedEmbedder`/`NoopEmbedder` and need
//! neither the model nor the network).
//!
//! ```bash
//! KEVIN_FASTEMBED_TESTS=1 cargo nextest run -p kevin-memory --features fastembed
//! ```

#![cfg(feature = "fastembed")]

use kevin_memory::embed::Embedder as _;
use kevin_memory::{EmbedderKind, FastEmbedEmbedder, MemoryCfg};

/// Returns from the test unless `KEVIN_FASTEMBED_TESTS=1` is set.
macro_rules! skip_unless_fastembed {
    () => {
        if std::env::var("KEVIN_FASTEMBED_TESTS").as_deref() != Ok("1") {
            eprintln!(
                "skipping {}: set KEVIN_FASTEMBED_TESTS=1 to run the fastembed model test",
                module_path!()
            );
            return;
        }
    };
}

fn cfg() -> MemoryCfg {
    let mut cfg =
        MemoryCfg::default().with_embedder(EmbedderKind::Fastembed, "BAAI/bge-small-en-v1.5");
    // Keep the cache out of the repository; CI restores this directory.
    if let Some(dir) = std::env::var_os("KEVIN_FASTEMBED_CACHE_DIR") {
        cfg.model_cache_dir = dir.into();
    } else {
        cfg.model_cache_dir = dirs_cache().join("kevin").join("embeddings");
    }
    cfg
}

fn dirs_cache() -> std::path::PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
}

#[tokio::test]
async fn ac_ws18_6_fastembed_model_loads_from_cache() {
    skip_unless_fastembed!();
    let cfg = cfg();

    let embedder = FastEmbedEmbedder::load(&cfg)
        .await
        .expect("the model loads (first run downloads it into the cache)");
    assert_eq!(embedder.dimensions(), 384);
    assert_eq!(embedder.model_name(), "BAAI/bge-small-en-v1.5");
    assert!(
        embedder.is_cache_warm(),
        "after a load the cache holds the model files: {}",
        embedder.cache_dir().display()
    );

    let vectors = embedder
        .embed_batch(&[
            "Run cargo fmt before opening pull requests".to_owned(),
            "Formatting must be applied before a PR is opened".to_owned(),
            "The cat sleeps on a warm roof".to_owned(),
        ])
        .await
        .expect("embed");
    assert_eq!(vectors.len(), 3);
    assert!(vectors.iter().all(|v| v.len() == 384));

    let cosine = |a: &[f32], b: &[f32]| -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    };
    assert!(
        cosine(&vectors[0], &vectors[1]) > cosine(&vectors[0], &vectors[2]),
        "paraphrases are closer than unrelated sentences"
    );

    // A second load is served entirely from the cache (no network).
    let again = FastEmbedEmbedder::load(&cfg)
        .await
        .expect("the model loads offline from the cache");
    assert_eq!(again.dimensions(), 384);
}
