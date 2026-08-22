//! The narrow port the evaluator uses to store lessons
//! (`plan/06-memory-and-learning.md` §3.4, auto-apply `memory`).
//!
//! Lessons are deduplicated against existing ones with cosine
//! ≥ [`LESSON_DEDUP_SIMILARITY`]: a near-duplicate supersedes the item it
//! duplicates instead of piling up a second copy.

use async_trait::async_trait;
use kevin_domain::MemoryItemId;
use kevin_memory::{MemoryKind, MemoryStore, SearchQuery, StoreRequest};
use std::sync::Mutex;

/// Cosine similarity above which a new lesson supersedes an existing one.
pub const LESSON_DEDUP_SIMILARITY: f32 = 0.92;

/// What the evaluator needs from memory.
#[async_trait]
pub trait MemoryPort: Send + Sync + std::fmt::Debug {
    /// The most similar existing lesson at or above `min_similarity`.
    async fn similar_lesson(
        &self,
        text: &str,
        min_similarity: f32,
    ) -> Result<Option<MemoryItemId>, MemoryPortError>;

    /// Stores a new item.
    async fn store(&self, req: StoreRequest) -> Result<MemoryItemId, MemoryPortError>;

    /// Stores `req` and marks `old` as superseded by it.
    async fn supersede(
        &self,
        old: MemoryItemId,
        req: StoreRequest,
    ) -> Result<MemoryItemId, MemoryPortError>;
}

/// Why a lesson could not be stored.
#[derive(Debug, thiserror::Error)]
#[error("memory: {0}")]
pub struct MemoryPortError(pub String);

impl MemoryPortError {
    /// Wraps any displayable error.
    pub fn new(err: impl std::fmt::Display) -> Self {
        Self(err.to_string())
    }
}

#[async_trait]
impl MemoryPort for MemoryStore {
    async fn similar_lesson(
        &self,
        text: &str,
        min_similarity: f32,
    ) -> Result<Option<MemoryItemId>, MemoryPortError> {
        let query = SearchQuery::new(text)
            .with_kinds([MemoryKind::Lesson])
            .with_top_k(1)
            .with_min_similarity(min_similarity);
        let hits = MemoryStore::search(self, query)
            .await
            .map_err(MemoryPortError::new)?;
        Ok(hits
            .into_iter()
            .find(|hit| hit.similarity >= min_similarity)
            .map(|hit| hit.item.id))
    }

    async fn store(&self, req: StoreRequest) -> Result<MemoryItemId, MemoryPortError> {
        MemoryStore::store(self, req)
            .await
            .map_err(MemoryPortError::new)
    }

    async fn supersede(
        &self,
        old: MemoryItemId,
        req: StoreRequest,
    ) -> Result<MemoryItemId, MemoryPortError> {
        MemoryStore::supersede(self, old, req)
            .await
            .map_err(MemoryPortError::new)
    }
}

/// An in-memory [`MemoryPort`] for tests: similarity is exact-text matching
/// after normalisation, which is enough to exercise the dedup branch without an
/// embedder.
#[derive(Debug, Default)]
pub struct InMemoryLessons {
    items: Mutex<Vec<(MemoryItemId, String, bool)>>,
}

impl InMemoryLessons {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Live (non-superseded) lesson texts, in insertion order.
    #[must_use]
    pub fn lessons(&self) -> Vec<String> {
        self.items
            .lock()
            .expect("lessons lock")
            .iter()
            .filter(|(_, _, superseded)| !superseded)
            .map(|(_, text, _)| text.clone())
            .collect()
    }

    /// Every lesson ever stored, superseded ones included.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        self.items
            .lock()
            .expect("lessons lock")
            .iter()
            .map(|(_, text, _)| text.clone())
            .collect()
    }
}

/// Lowercased, whitespace-collapsed text.
fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[async_trait]
impl MemoryPort for InMemoryLessons {
    async fn similar_lesson(
        &self,
        text: &str,
        _min_similarity: f32,
    ) -> Result<Option<MemoryItemId>, MemoryPortError> {
        let needle = normalise(text);
        Ok(self
            .items
            .lock()
            .expect("lessons lock")
            .iter()
            .find(|(_, stored, superseded)| !superseded && normalise(stored) == needle)
            .map(|(id, _, _)| *id))
    }

    async fn store(&self, req: StoreRequest) -> Result<MemoryItemId, MemoryPortError> {
        let id = MemoryItemId::new();
        self.items
            .lock()
            .expect("lessons lock")
            .push((id, req.content, false));
        Ok(id)
    }

    async fn supersede(
        &self,
        old: MemoryItemId,
        req: StoreRequest,
    ) -> Result<MemoryItemId, MemoryPortError> {
        let id = MemoryItemId::new();
        let mut items = self.items.lock().expect("lessons lock");
        if let Some(entry) = items.iter_mut().find(|(existing, _, _)| *existing == old) {
            entry.2 = true;
        }
        items.push((id, req.content, false));
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_duplicate_lesson_supersedes_instead_of_piling_up() {
        let store = InMemoryLessons::new();
        let first = store
            .store(StoreRequest::lesson("run cargo fmt before reporting"))
            .await
            .unwrap();
        let found = store
            .similar_lesson("Run cargo fmt   before reporting", LESSON_DEDUP_SIMILARITY)
            .await
            .unwrap();
        assert_eq!(found, Some(first));
        store
            .supersede(
                first,
                StoreRequest::lesson("run cargo fmt before reporting"),
            )
            .await
            .unwrap();
        assert_eq!(store.lessons().len(), 1);
        assert_eq!(store.all().len(), 2);
    }
}
