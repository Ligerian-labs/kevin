//! The summariser contract (`plan/06-memory-and-learning.md` §1.5).
//!
//! Run summaries, artifact summaries and preferences are produced by a single
//! structured call. The call itself is a worker call (role `default`, effort
//! low) — but `kevin-memory` must not depend on `kevin-worker`, so this module
//! defines only the contract ([`Summarizer`], [`SummaryRequest`],
//! [`SummaryOutput`], the prompt and the JSON schema) plus a deterministic
//! default, [`ExtractiveSummarizer`].
//!
//! The orchestrator wires the worker-backed implementation:
//!
//! ```ignore
//! struct WorkerSummarizer { worker: Arc<dyn kevin_worker::Worker>, /* role, effort */ }
//! #[async_trait] impl kevin_memory::Summarizer for WorkerSummarizer { … }
//! ```
// TODO(ws-08/ws-10): the worker-backed `Summarizer` lives in the orchestrator
// (role `default`, effort low, schema `summary_json_schema()`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::Result;
use crate::item::{MemoryKind, MemoryScope, MemorySource};
use crate::store::StoreRequest;

/// System prompt of the summariser (`plan/06` §1.5, verbatim).
pub const SUMMARIZER_SYSTEM_PROMPT: &str = "You write terse memory records for an agent runtime. \
Summarise what happened, never what was planned. Extract a preference only if the human's answer \
would change how a *future, different* task should be done; otherwise return an empty list. \
Never include credentials, tokens, URLs with query strings, or personal data.";

/// Preferences below this confidence are discarded.
pub const MIN_PREFERENCE_CONFIDENCE: f32 = 0.7;

/// Maximum length of a run summary.
pub const MAX_SUMMARY_CHARS: usize = 600;
/// Maximum length of one artifact summary.
pub const MAX_ARTIFACT_SUMMARY_CHARS: usize = 300;
/// Maximum length of a preference statement.
pub const MAX_PREFERENCE_CHARS: usize = 200;

/// The structured-output schema the summariser call must satisfy.
#[must_use]
pub fn summary_json_schema() -> Value {
    json!({
        "type": "object",
        "required": ["summary", "preferences"],
        "properties": {
            "summary": { "type": "string", "maxLength": MAX_SUMMARY_CHARS },
            "artifact_summaries": { "type": "array", "items": { "type": "object",
                "required": ["artifact_id", "summary"],
                "properties": {
                    "artifact_id": { "type": "string" },
                    "summary": { "type": "string", "maxLength": MAX_ARTIFACT_SUMMARY_CHARS }
                } } },
            "preferences": { "type": "array", "items": { "type": "object",
                "required": ["statement", "confidence"],
                "properties": {
                    "statement": { "type": "string", "maxLength": MAX_PREFERENCE_CHARS },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 },
                    "scope": { "type": "string", "enum": ["global", "repo"] }
                } } }
        }
    })
}

/// One artifact to summarise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInput {
    /// Stable artifact id (path or artifact uuid).
    pub artifact_id: String,
    /// What is known about it (description, first lines, `git diff --stat`…).
    pub text: String,
}

/// What the summariser is asked to condense.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SummaryRequest {
    /// The run's goal.
    pub goal: String,
    /// What happened (integration result, verdicts, notable decisions).
    pub outcome: String,
    /// Artifacts worth their own summary.
    #[serde(default)]
    pub artifacts: Vec<ArtifactInput>,
    /// Answers a human gave during the run (preference extraction input).
    #[serde(default)]
    pub human_answers: Vec<String>,
    /// Repository the run happened in, as a scope label (`repo:<id>`/`global`).
    #[serde(default)]
    pub repo_scope: Option<String>,
}

impl SummaryRequest {
    /// A request for a run with a goal and an outcome.
    pub fn new(goal: impl Into<String>, outcome: impl Into<String>) -> Self {
        Self {
            goal: goal.into(),
            outcome: outcome.into(),
            ..Self::default()
        }
    }

    /// Adds artifacts.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactInput>) -> Self {
        self.artifacts = artifacts;
        self
    }

    /// Adds human answers (preference candidates).
    #[must_use]
    pub fn with_human_answers(mut self, answers: Vec<String>) -> Self {
        self.human_answers = answers;
        self
    }
}

/// One artifact summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    /// Which artifact.
    pub artifact_id: String,
    /// ≤ 300 characters: what it is, where, why.
    pub summary: String,
}

/// Where an extracted preference applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreferenceScope {
    /// Applies everywhere.
    #[default]
    Global,
    /// Applies to the run's repository only.
    Repo,
}

/// One extracted preference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preference {
    /// "User prefers X when Y".
    pub statement: String,
    /// Confidence in `0..=1`; below [`MIN_PREFERENCE_CONFIDENCE`] it is dropped.
    pub confidence: f32,
    /// Scope hint.
    #[serde(default)]
    pub scope: PreferenceScope,
}

/// The summariser's structured output.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SummaryOutput {
    /// ≤ 600 characters: goal, what was done, outcome, notable decisions.
    pub summary: String,
    /// Per-artifact summaries.
    #[serde(default)]
    pub artifact_summaries: Vec<ArtifactSummary>,
    /// Extracted preferences (already filtered by the caller, see
    /// [`SummaryOutput::store_requests`]).
    #[serde(default)]
    pub preferences: Vec<Preference>,
}

impl SummaryOutput {
    /// Turns the output into the items memory should store: one `run_summary`,
    /// one `artifact_summary` per artifact and one `preference` per statement
    /// above [`MIN_PREFERENCE_CONFIDENCE`].
    ///
    /// `repo` is the run's repository scope; preferences marked `global` are
    /// stored globally.
    #[must_use]
    pub fn store_requests(&self, source: &MemorySource, repo: &MemoryScope) -> Vec<StoreRequest> {
        let mut requests = Vec::new();
        if !self.summary.trim().is_empty() {
            requests.push(
                StoreRequest::new(
                    MemoryKind::RunSummary,
                    cap(&self.summary, MAX_SUMMARY_CHARS),
                )
                .with_scope(repo.clone())
                .with_source(source.clone()),
            );
        }
        for artifact in &self.artifact_summaries {
            requests.push(
                StoreRequest::new(
                    MemoryKind::ArtifactSummary,
                    cap(&artifact.summary, MAX_ARTIFACT_SUMMARY_CHARS),
                )
                .with_scope(repo.clone())
                .with_source(source.clone())
                .with_tags([artifact.artifact_id.clone()]),
            );
        }
        for preference in &self.preferences {
            if preference.confidence < MIN_PREFERENCE_CONFIDENCE {
                continue;
            }
            let scope = match preference.scope {
                PreferenceScope::Global => MemoryScope::Global,
                PreferenceScope::Repo => repo.clone(),
            };
            requests.push(
                StoreRequest::new(
                    MemoryKind::Preference,
                    cap(&preference.statement, MAX_PREFERENCE_CHARS),
                )
                .with_scope(scope)
                .with_source(source.clone()),
            );
        }
        requests
    }
}

/// Produces run/artifact summaries and preferences. Implemented by the
/// orchestrator against a worker; [`ExtractiveSummarizer`] is the default.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Summarises one run.
    async fn summarize(&self, request: SummaryRequest) -> Result<SummaryOutput>;
}

/// Deterministic, model-free summariser: keeps the first sentences (and
/// headings) of the outcome, caps every field, and extracts no preference.
///
/// It is the default so that memory works with `memory.enabled = true` before
/// any worker is wired, and it makes summary tests deterministic.
#[derive(Debug, Clone, Copy)]
pub struct ExtractiveSummarizer {
    /// Sentences/headings kept from the outcome.
    pub sentences: usize,
}

impl Default for ExtractiveSummarizer {
    fn default() -> Self {
        Self { sentences: 3 }
    }
}

impl ExtractiveSummarizer {
    /// A summariser keeping `sentences` sentences.
    #[must_use]
    pub const fn new(sentences: usize) -> Self {
        Self { sentences }
    }
}

#[async_trait]
impl Summarizer for ExtractiveSummarizer {
    async fn summarize(&self, request: SummaryRequest) -> Result<SummaryOutput> {
        let body = first_sentences(&request.outcome, self.sentences);
        let goal = request.goal.trim();
        let summary = if goal.is_empty() {
            body
        } else if body.is_empty() {
            format!("Goal: {goal}.")
        } else {
            format!("Goal: {goal}. {body}")
        };
        let artifact_summaries = request
            .artifacts
            .iter()
            .map(|artifact| ArtifactSummary {
                artifact_id: artifact.artifact_id.clone(),
                summary: cap(
                    &first_sentences(&artifact.text, 1),
                    MAX_ARTIFACT_SUMMARY_CHARS,
                ),
            })
            .collect();
        Ok(SummaryOutput {
            summary: cap(&summary, MAX_SUMMARY_CHARS),
            artifact_summaries,
            // A preference needs judgement about generalisation: the
            // extractive default never guesses (plan/06 §1.5).
            preferences: Vec::new(),
        })
    }
}

/// The first `count` sentences or headings of `text`, whitespace-normalised.
///
/// A sentence ends on `.`/`!`/`?` **followed by whitespace or the end of the
/// line**, so `core.events` or `v1.5` do not split a sentence in two.
fn first_sentences(text: &str, count: usize) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(heading) = line.strip_prefix('#') {
            out.push(heading.trim_start_matches('#').trim().to_owned());
        } else {
            for sentence in sentences(line) {
                out.push(sentence);
                if out.len() >= count {
                    break;
                }
            }
        }
        if out.len() >= count {
            break;
        }
    }
    out.truncate(count);
    out.join(" ")
}

/// Splits one line into sentences on terminal punctuation.
fn sentences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    for (i, (offset, c)) in chars.iter().enumerate() {
        if !matches!(c, '.' | '!' | '?') {
            continue;
        }
        let ends = chars
            .get(i + 1)
            .is_none_or(|(_, next)| next.is_whitespace());
        if !ends {
            continue;
        }
        let end = offset + c.len_utf8();
        let sentence = line[start..end].trim();
        if !sentence.is_empty() {
            out.push(sentence.to_owned());
        }
        start = end;
    }
    let tail = line[start..].trim();
    if !tail.is_empty() {
        out.push(tail.to_owned());
    }
    out
}

fn cap(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use kevin_domain::Actor;

    use super::*;
    use crate::item::RepoId;

    #[tokio::test]
    async fn extractive_summariser_is_deterministic_and_capped() {
        let summarizer = ExtractiveSummarizer::default();
        let request = SummaryRequest::new(
            "add the event store",
            "# Result\nAdded core.events and the outbox. Tests run against Postgres. \
             Follow-up: prune job.\nMore detail that must not appear.",
        )
        .with_artifacts(vec![ArtifactInput {
            artifact_id: "crates/kevin-store/src/event_store.rs".to_owned(),
            text: "Append with optimistic concurrency. Everything else.".to_owned(),
        }]);
        let first = summarizer.summarize(request.clone()).await.unwrap();
        let second = summarizer.summarize(request).await.unwrap();
        assert_eq!(first, second);
        assert!(first.summary.starts_with("Goal: add the event store."));
        assert!(first.summary.contains("Added core.events"));
        assert!(!first.summary.contains("must not appear"));
        assert!(first.summary.chars().count() <= MAX_SUMMARY_CHARS);
        assert_eq!(first.artifact_summaries.len(), 1);
        assert_eq!(
            first.artifact_summaries[0].summary,
            "Append with optimistic concurrency."
        );
        assert!(first.summary.contains("Tests run against Postgres."));
        assert!(first.preferences.is_empty());
    }

    #[test]
    fn store_requests_filter_low_confidence_preferences_and_pick_scopes() {
        let repo = RepoId::from_origin("https://example.com/x").scope();
        let output = SummaryOutput {
            summary: "Did the thing.".to_owned(),
            artifact_summaries: vec![ArtifactSummary {
                artifact_id: "a.rs".to_owned(),
                summary: "A file.".to_owned(),
            }],
            preferences: vec![
                Preference {
                    statement: "User prefers jj bookmarks".to_owned(),
                    confidence: 0.9,
                    scope: PreferenceScope::Global,
                },
                Preference {
                    statement: "Unsure".to_owned(),
                    confidence: 0.5,
                    scope: PreferenceScope::Repo,
                },
            ],
        };
        let requests =
            output.store_requests(&MemorySource::from_actor(Actor::system("test")), &repo);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].kind, MemoryKind::RunSummary);
        assert_eq!(requests[0].scope, repo);
        assert_eq!(requests[1].kind, MemoryKind::ArtifactSummary);
        assert_eq!(requests[1].tags, vec!["a.rs".to_owned()]);
        assert_eq!(requests[2].kind, MemoryKind::Preference);
        assert_eq!(requests[2].scope, MemoryScope::Global);
    }

    #[test]
    fn the_schema_matches_the_plan() {
        let schema = summary_json_schema();
        assert_eq!(schema["required"], json!(["summary", "preferences"]));
        assert_eq!(schema["properties"]["summary"]["maxLength"], json!(600));
        assert_eq!(
            schema["properties"]["preferences"]["items"]["required"],
            json!(["statement", "confidence"])
        );
    }
}
