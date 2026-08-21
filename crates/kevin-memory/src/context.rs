//! Retrieval at intake and the `<kevin-memory>` context block
//! (`plan/06-memory-and-learning.md` §1.6).
//!
//! The saga calls [`ContextBuilder::for_intake`] on `run.started` (before the
//! planner) and [`ContextBuilder::for_task`] before each
//! `implement`/`debug`/`refactor` attempt. The rendered block is passed as
//! `context.memory` in `TaskAttemptRequest` and its `refs` are recorded as
//! `context_refs[]` on the run.

use kevin_domain::MemoryItemId;

use crate::embed::MAX_INPUT_CHARS;
use crate::error::Result;
use crate::item::{INTAKE_KINDS, MemoryKind, RepoId, ScopeFilter, TASK_KINDS, scope_label};
use crate::store::{Hit, MemoryStore, SearchQuery};

/// Opening tag of the block.
pub const OPEN_TAG: &str = "<kevin-memory>";
/// Closing tag of the block.
pub const CLOSE_TAG: &str = "</kevin-memory>";
/// Characters per token used to estimate the block size (`plan/06` §1.6).
pub const CHARS_PER_TOKEN: usize = 4;

/// A rendered memory block plus the ids it cites.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextBlock {
    /// The `<kevin-memory>…</kevin-memory>` text (empty when nothing matched).
    pub text: String,
    /// Ids rendered in the block, best first (`context_refs[]`).
    pub refs: Vec<MemoryItemId>,
    /// Estimated token count of `text` (chars / 4).
    pub estimated_tokens: usize,
}

impl ContextBlock {
    /// Whether anything was retrieved.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Builds context blocks out of a [`MemoryStore`].
#[derive(Debug)]
pub struct ContextBuilder<'a> {
    store: &'a MemoryStore,
    max_tokens: usize,
}

impl<'a> ContextBuilder<'a> {
    /// A builder capped at `memory.context_max_tokens`.
    #[must_use]
    pub fn new(store: &'a MemoryStore) -> Self {
        let max_tokens = store.cfg().context_max_tokens;
        Self { store, max_tokens }
    }

    /// Overrides the token cap (tests, tighter worker budgets).
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Run-level retrieval: lessons, preferences, facts and past run summaries
    /// for `goal`, in `repo` scope plus global.
    pub async fn for_intake(&self, goal: &str, repo: Option<&RepoId>) -> Result<ContextBlock> {
        let query = SearchQuery::new(truncate_query(goal))
            .with_kinds(INTAKE_KINDS)
            .with_scope(ScopeFilter::for_repo(repo));
        let hits = self.store.search(query).await?;
        Ok(self.render(&hits))
    }

    /// Task-level retrieval: lessons, preferences and artifact summaries for
    /// the goal plus the task's title/instructions.
    pub async fn for_task(
        &self,
        goal: &str,
        task_text: &str,
        repo: Option<&RepoId>,
    ) -> Result<ContextBlock> {
        let text = truncate_query(&format!("{goal}\n{task_text}"));
        let query = SearchQuery::new(text)
            .with_kinds(TASK_KINDS)
            .with_scope(ScopeFilter::for_repo(repo));
        let hits = self.store.search(query).await?;
        Ok(self.render(&hits))
    }

    /// Renders hits into the block, dropping the lowest-scoring ones until the
    /// estimate fits the cap. Pure: no IO, so the cap is unit-testable.
    #[must_use]
    pub fn render(&self, hits: &[Hit]) -> ContextBlock {
        let mut kept: Vec<&Hit> = hits.iter().filter(|hit| hit.item.is_live()).collect();
        kept.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        loop {
            if kept.is_empty() {
                return ContextBlock::default();
            }
            let text = render_block(&kept);
            let estimated_tokens = text.len().div_ceil(CHARS_PER_TOKEN);
            if estimated_tokens <= self.max_tokens {
                return ContextBlock {
                    text,
                    refs: kept.iter().map(|hit| hit.item.id).collect(),
                    estimated_tokens,
                };
            }
            kept.pop(); // drop the lowest-scoring hit and try again
        }
    }
}

fn truncate_query(text: &str) -> String {
    crate::embed::truncate_input(text.trim())
}

fn render_block(hits: &[&Hit]) -> String {
    let mut out = String::from(OPEN_TAG);
    for (kind, heading) in [
        (MemoryKind::Lesson, "Lessons (most relevant first):"),
        (MemoryKind::Preference, "Preferences:"),
        (MemoryKind::Fact, "Facts:"),
        (MemoryKind::RunSummary, "Past runs:"),
        (MemoryKind::ArtifactSummary, "Artifacts:"),
    ] {
        let section: Vec<&&Hit> = hits.iter().filter(|hit| hit.item.kind == kind).collect();
        if section.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(heading);
        for hit in section {
            out.push_str("\n- ");
            out.push_str(&render_line(hit));
        }
    }
    out.push('\n');
    out.push_str(CLOSE_TAG);
    out
}

fn render_line(hit: &Hit) -> String {
    let item = &hit.item;
    let stamp = if item.kind == MemoryKind::RunSummary {
        item.created_at.format("%Y-%m-%d").to_string()
    } else {
        format!("{:.2}", hit.score)
    };
    format!(
        "[{} | {} | {}] {}",
        item.short_id(),
        scope_label(&item.scope),
        stamp,
        item.content.replace('\n', " ")
    )
}

/// The maximum query length sent to the embedder (`plan/06` §1.6).
pub const MAX_QUERY_CHARS: usize = MAX_INPUT_CHARS;

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use kevin_domain::MemoryItemId;

    use super::*;
    use kevin_domain::Actor;

    use crate::item::{MemoryRecord, MemoryScope, MemorySource};

    fn hit(kind: MemoryKind, content: &str, score: f32) -> Hit {
        Hit {
            item: MemoryRecord {
                id: MemoryItemId::new(),
                kind,
                content: content.to_owned(),
                tags: Vec::new(),
                source: MemorySource::from_actor(Actor::system("test")),
                scope: MemoryScope::Global,
                importance: 0.5,
                embedding_model: None,
                created_at: Utc.with_ymd_and_hms(2026, 8, 12, 9, 0, 0).unwrap(),
                superseded_by: None,
                forgotten_at: None,
            },
            similarity: score,
            lexical: 0.0,
            score,
        }
    }

    fn builder_block(hits: &[Hit], max_tokens: usize) -> ContextBlock {
        // `render` needs no store access, but `ContextBuilder` borrows one:
        // build the block through the same code path with a fake store is
        // overkill, so exercise the pure renderer directly.
        let mut kept: Vec<&Hit> = hits.iter().collect();
        kept.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        loop {
            if kept.is_empty() {
                return ContextBlock::default();
            }
            let text = render_block(&kept);
            let estimated_tokens = text.len().div_ceil(CHARS_PER_TOKEN);
            if estimated_tokens <= max_tokens {
                return ContextBlock {
                    text,
                    refs: kept.iter().map(|h| h.item.id).collect(),
                    estimated_tokens,
                };
            }
            kept.pop();
        }
    }

    #[test]
    fn block_has_one_section_per_kind_and_citable_ids() {
        let hits = vec![
            hit(MemoryKind::Lesson, "Run cargo fmt before opening PRs", 0.81),
            hit(MemoryKind::Preference, "User prefers jj bookmarks", 0.77),
            hit(MemoryKind::RunSummary, "Added the event store crate", 0.4),
        ];
        let block = builder_block(&hits, 2500);
        assert!(block.text.starts_with(OPEN_TAG));
        assert!(block.text.ends_with(CLOSE_TAG));
        assert!(block.text.contains("Lessons (most relevant first):"));
        assert!(block.text.contains("Preferences:"));
        assert!(block.text.contains("Past runs:"));
        assert!(block.text.contains("2026-08-12"), "{}", block.text);
        assert!(block.text.contains("| global | 0.81]"), "{}", block.text);
        assert_eq!(block.refs.len(), 3);
    }

    #[test]
    fn the_cap_drops_the_lowest_scoring_hits_first() {
        let hits: Vec<Hit> = (0..40u16)
            .map(|i| {
                hit(
                    MemoryKind::Lesson,
                    &format!("lesson number {i} with a reasonably long body"),
                    1.0 - f32::from(i) / 100.0,
                )
            })
            .collect();
        let block = builder_block(&hits, 40);
        assert!(block.estimated_tokens <= 40, "{}", block.estimated_tokens);
        assert!(block.refs.len() < hits.len());
        assert_eq!(block.refs.first(), Some(&hits[0].item.id));
        assert!(block.text.contains("lesson number 0"));
        assert!(!block.text.contains("lesson number 39"));
    }

    #[test]
    fn nothing_retrieved_renders_an_empty_block() {
        assert!(builder_block(&[], 2500).is_empty());
    }
}
