//! Storage-side memory values (`plan/02-domain-model.md` §Memory,
//! `plan/06-memory-and-learning.md` §1.1).
//!
//! The vocabulary itself — [`MemoryKind`], [`MemoryScope`], [`MemorySource`],
//! the `MemoryItem` **aggregate** and its commands/events — belongs to
//! `kevin-domain` (WS-01) and is re-exported here. This module adds only what
//! storage and retrieval need on top: the canonical [`RepoId`], the
//! [`ScopeFilter`] of a query and [`MemoryRecord`], the row of
//! `memory.memory_items` (the aggregate never sees `forgotten_at`, the vector
//! or the tsvector).

use std::fmt;
use std::path::Path;

use chrono::{DateTime, Utc};
use kevin_domain::{Actor, MemoryItemId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub use kevin_domain::memory_item::MAX_CONTENT_CHARS;
pub use kevin_domain::values::{MemoryKind, MemoryScope, MemorySource};

/// The kinds retrieved for the intake (run-level) query (`plan/06` §1.6).
pub const INTAKE_KINDS: [MemoryKind; 4] = [
    MemoryKind::Lesson,
    MemoryKind::Preference,
    MemoryKind::Fact,
    MemoryKind::RunSummary,
];

/// The kinds retrieved before a task attempt (`plan/06` §1.6).
pub const TASK_KINDS: [MemoryKind; 3] = [
    MemoryKind::Lesson,
    MemoryKind::Preference,
    MemoryKind::ArtifactSummary,
];

/// `kind` was not one of the five known spellings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown memory kind `{0}` (expected one of lesson, preference, fact, run_summary, artifact_summary)"
)]
pub struct UnknownMemoryKind(pub String);

/// Parses the stored spelling of a kind (`kevin_domain::MemoryKind` has no
/// `FromStr`; the database and the CLI both need one).
pub fn parse_kind(value: &str) -> Result<MemoryKind, UnknownMemoryKind> {
    MemoryKind::ALL
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or_else(|| UnknownMemoryKind(value.to_owned()))
}

/// One-letter prefix of the short id used in the context block (`L-3f2a`).
#[must_use]
pub const fn kind_short_prefix(kind: MemoryKind) -> char {
    match kind {
        MemoryKind::Lesson => 'L',
        MemoryKind::Preference => 'P',
        MemoryKind::Fact => 'F',
        MemoryKind::RunSummary => 'R',
        MemoryKind::ArtifactSummary => 'A',
    }
}

/// Short label of a scope for the context block and the CLI.
#[must_use]
pub const fn scope_label(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Global => "global",
        MemoryScope::Repo(_) => "repo",
    }
}

/// Canonical repository identifier: sha256 of the canonical origin URL when
/// available, else of the absolute repository root path (`plan/06` §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoId(String);

impl RepoId {
    /// From a git origin URL (trailing `/`, `.git` and case are normalised).
    #[must_use]
    pub fn from_origin(url: &str) -> Self {
        let canonical = url
            .trim()
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .to_lowercase();
        Self::hash(&canonical)
    }

    /// From an absolute repository root path.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        Self::hash(&path.to_string_lossy())
    }

    /// Wraps an already-computed id (e.g. read back from the database).
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// The hex digest.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First 12 hex characters — what the CLI prints.
    #[must_use]
    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(12)]
    }

    /// The matching `repo:<id>` scope.
    #[must_use]
    pub fn scope(&self) -> MemoryScope {
        MemoryScope::Repo(self.0.clone())
    }

    fn hash(input: &str) -> Self {
        let digest = Sha256::digest(input.as_bytes());
        Self(format!("{digest:x}"))
    }
}

impl fmt::Display for RepoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Scope restriction of a search.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ScopeFilter {
    /// Global items only.
    #[default]
    Global,
    /// One repository's items only (Kohral mode forces this).
    Repo(RepoId),
    /// The repository's items plus the global ones (intake default).
    RepoAndGlobal(RepoId),
}

impl ScopeFilter {
    /// The repository's items plus the global ones, or global only when no
    /// repository is known.
    #[must_use]
    pub fn for_repo(repo: Option<&RepoId>) -> Self {
        repo.map_or(ScopeFilter::Global, |id| {
            ScopeFilter::RepoAndGlobal(id.clone())
        })
    }

    /// The `scope` values this filter accepts (bound as `scope = ANY($n)`).
    #[must_use]
    pub fn scopes(&self) -> Vec<String> {
        match self {
            ScopeFilter::Global => vec![MemoryScope::Global.to_string()],
            ScopeFilter::Repo(id) => vec![id.scope().to_string()],
            ScopeFilter::RepoAndGlobal(id) => {
                vec![id.scope().to_string(), MemoryScope::Global.to_string()]
            }
        }
    }

    /// The repository this filter is about, when any.
    #[must_use]
    pub const fn repo(&self) -> Option<&RepoId> {
        match self {
            ScopeFilter::Global => None,
            ScopeFilter::Repo(id) | ScopeFilter::RepoAndGlobal(id) => Some(id),
        }
    }
}

/// One row of `memory.memory_items` (without the vector).
///
/// The event-sourced state of the same item is
/// [`kevin_domain::MemoryItem`]; this is the projection storage and retrieval
/// work with — the one context where the aggregate table *is* the read model
/// (`plan/06` §1.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Item id.
    pub id: MemoryItemId,
    /// Kind.
    pub kind: MemoryKind,
    /// Content (blank once forgotten).
    pub content: String,
    /// Free-form tags (task kind, repo id, worker…).
    pub tags: Vec<String>,
    /// Provenance.
    pub source: MemorySource,
    /// Scope.
    pub scope: MemoryScope,
    /// Importance in `0..=1`.
    pub importance: f32,
    /// Model that produced the stored embedding (`None` when not embedded).
    pub embedding_model: Option<String>,
    /// When the item was stored.
    pub created_at: DateTime<Utc>,
    /// The item that replaced this one, if any.
    pub superseded_by: Option<MemoryItemId>,
    /// When the item was forgotten, if it was.
    pub forgotten_at: Option<DateTime<Utc>>,
}

impl MemoryRecord {
    /// Short citable id used in the `<kevin-memory>` block (`L-3f2a`).
    #[must_use]
    pub fn short_id(&self) -> String {
        let hex = self.id.as_uuid().simple().to_string();
        format!("{}-{}", kind_short_prefix(self.kind), &hex[..4])
    }

    /// Whether the item is still retrievable.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.forgotten_at.is_none() && self.superseded_by.is_none()
    }

    /// Who stored it.
    #[must_use]
    pub const fn actor(&self) -> &Actor {
        &self.source.actor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_through_their_stored_spelling() {
        for kind in MemoryKind::ALL {
            assert_eq!(parse_kind(kind.as_str()).unwrap(), kind);
        }
        assert!(parse_kind("nope").is_err());
        assert!((MemoryKind::Preference.default_importance() - 0.8).abs() < f32::EPSILON);
        assert_eq!(kind_short_prefix(MemoryKind::RunSummary), 'R');
    }

    #[test]
    fn repo_id_is_stable_and_canonicalised() {
        let a = RepoId::from_origin("https://github.com/Ligerian-labs/kevin.git");
        let b = RepoId::from_origin("https://github.com/Ligerian-labs/Kevin/");
        assert_eq!(a, b);
        assert_eq!(a.as_str().len(), 64);
        assert_eq!(a.short().len(), 12);
        assert_ne!(a, RepoId::from_path(Path::new("/tmp/kevin")));
        assert_eq!(a.scope().to_string(), format!("repo:{a}"));
    }

    #[test]
    fn scope_filters_expand_to_the_stored_spellings() {
        let repo = RepoId::from_origin("git@github.com:x/y.git");
        assert_eq!(
            ScopeFilter::RepoAndGlobal(repo.clone()).scopes(),
            vec![format!("repo:{repo}"), "global".to_owned()]
        );
        assert_eq!(
            ScopeFilter::Repo(repo.clone()).scopes(),
            vec![format!("repo:{repo}")]
        );
        assert_eq!(ScopeFilter::Global.repo(), None);
        assert_eq!(ScopeFilter::for_repo(Some(&repo)).repo(), Some(&repo));
        assert_eq!(scope_label(&repo.scope()), "repo");
        assert_eq!(scope_label(&MemoryScope::Global), "global");
    }
}
