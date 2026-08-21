//! The summariser: run and artifact summaries plus preference extraction, the
//! single call described in `plan/06-memory-and-learning.md` §1.5.

use kevin_domain::{MemoryScope, TaskKind};
use serde::{Deserialize, Serialize};

use super::context::RoleContext;
use super::{Role, RoleError, RoleRequest, build_request, deserialize, extract, schemas, vars_of};

const SYSTEM: &str = include_str!("../../prompts/summarizer.system.md");
const USER: &str = include_str!("../../prompts/summarizer.user.md");

/// Confidence below which a preference is discarded
/// (`plan/06-memory-and-learning.md` §1.5).
pub const MIN_PREFERENCE_CONFIDENCE: f32 = 0.7;

/// Where a preference applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceScope {
    /// Everywhere.
    #[default]
    Global,
    /// This repository only.
    Repo,
}

impl PreferenceScope {
    /// The domain scope, given the current repository id.
    #[must_use]
    pub fn to_memory_scope(self, repo_id: &str) -> MemoryScope {
        match self {
            PreferenceScope::Global => MemoryScope::Global,
            PreferenceScope::Repo => MemoryScope::Repo(repo_id.to_owned()),
        }
    }
}

/// One summarised artifact (`artifact_summary` memory item).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSummary {
    /// Echoed from the input.
    pub artifact_id: String,
    /// ≤ 300 characters.
    pub summary: String,
}

/// One extracted preference (`preference` memory item).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreferenceRecord {
    /// "User prefers X when Y", ≤ 200 characters.
    pub statement: String,
    /// 0..=1; below [`MIN_PREFERENCE_CONFIDENCE`] the record is discarded.
    pub confidence: f32,
    /// Global or repository-scoped.
    #[serde(default)]
    pub scope: PreferenceScope,
}

/// The summariser's output (`kevin.summary.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRecords {
    /// ≤ 600 characters: the `run_summary` memory item.
    pub summary: String,
    /// One entry per artifact given.
    #[serde(default)]
    pub artifact_summaries: Vec<ArtifactSummary>,
    /// Preferences worth remembering.
    pub preferences: Vec<PreferenceRecord>,
}

impl MemoryRecords {
    /// The preferences at or above `min_confidence` (use
    /// [`MIN_PREFERENCE_CONFIDENCE`]); the rest are dropped.
    #[must_use]
    pub fn kept_preferences(&self, min_confidence: f32) -> Vec<&PreferenceRecord> {
        self.preferences
            .iter()
            .filter(|p| p.confidence >= min_confidence)
            .collect()
    }
}

/// Run and artifact summaries, preference extraction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summarizer;

impl Role for Summarizer {
    type Output = MemoryRecords;

    fn name(&self) -> &'static str {
        "summarizer"
    }

    fn task_kind(&self) -> TaskKind {
        TaskKind::Write
    }

    fn build(&self, ctx: &RoleContext) -> RoleRequest {
        build_request(
            SYSTEM,
            USER,
            vars_of(ctx),
            schemas::summary().clone(),
            schemas::SUMMARY_V1_ID,
        )
    }

    fn parse(&self, raw: &str) -> Result<MemoryRecords, RoleError> {
        let role = self.name();
        let value = extract(role, raw, schemas::summary())?;
        deserialize(role, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_confidence_preferences_are_dropped() {
        let raw = "```json\n{\"summary\": \"did the thing\", \"preferences\": [\
            {\"statement\": \"User prefers jj\", \"confidence\": 0.9, \"scope\": \"global\"},\
            {\"statement\": \"User prefers tabs\", \"confidence\": 0.4}]}\n```";
        let records = Summarizer.parse(raw).unwrap();
        assert_eq!(records.kept_preferences(MIN_PREFERENCE_CONFIDENCE).len(), 1);
        assert_eq!(records.preferences[1].scope, PreferenceScope::Global);
        assert_eq!(
            PreferenceScope::Repo.to_memory_scope("abc"),
            MemoryScope::Repo("abc".to_owned())
        );
    }

    #[test]
    fn a_summary_longer_than_the_schema_allows_is_a_schema_violation() {
        let raw = format!(
            "{{\"summary\": \"{}\", \"preferences\": []}}",
            "x".repeat(601)
        );
        assert!(Summarizer.parse(&raw).unwrap_err().is_schema_violation());
    }
}
