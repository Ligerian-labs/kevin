//! Evidence handed to the judge (`plan/06-memory-and-learning.md` §3.2).
//!
//! In this order, each section capped: task spec + acceptance criteria (full) ·
//! the diff or artifact list (≤ [`DIFF_CAP`]; a larger diff is summarised per
//! file with a `git diff --stat`-style header plus [`DIFF_FILE_LINES`] lines per
//! file) · test/command outputs (≤ [`TEST_OUTPUT_CAP`], tail) · transcript
//! summary (≤ [`TRANSCRIPT_CAP`]) · usage and cost · for run-level: the plan,
//! per-task verdicts and the integration result.
//!
//! Anti-gaming: everything the judge reads goes through [`Scrubber`] first, so
//! the model alias, the provider model id and the worker name never reach it.

use kevin_domain::{Route, Usage, Verdict};
use rust_decimal::Decimal;

/// Cap on the diff / artifact section, in characters.
pub const DIFF_CAP: usize = 40_000;
/// Lines kept per file when a diff is summarised.
pub const DIFF_FILE_LINES: usize = 200;
/// Cap on the captured test/command output, in characters (tail).
pub const TEST_OUTPUT_CAP: usize = 8_000;
/// Cap on the transcript summary, in characters.
pub const TRANSCRIPT_CAP: usize = 2_000;
/// What a redacted route mention is replaced with.
pub const REDACTED: &str = "[redacted-route]";

/// One task's verdict, listed in a run-level evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskVerdict {
    /// Task title.
    pub title: String,
    /// Task kind, as a string (the judge never sees the route).
    pub kind: String,
    /// The verdict its own evaluation recorded, when it had one.
    pub verdict: Option<Verdict>,
    /// Its overall score, when it had one.
    pub overall: Option<f32>,
}

/// An artifact the work produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactLine {
    /// What it is (`diff`, `pr`, `file`, …).
    pub kind: String,
    /// Where it lives.
    pub uri: String,
    /// One line for a reader.
    pub description: Option<String>,
}

/// Everything a judge is shown about one subject.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evidence {
    /// The task spec (or the run goal), verbatim.
    pub task_spec: String,
    /// Acceptance criteria, verbatim.
    pub acceptance_criteria: Vec<String>,
    /// Run success criteria from the understanding (run-level).
    pub success_criteria: Vec<String>,
    /// The approved plan (run-level).
    pub plan: Option<String>,
    /// Unified diff of the result, when there is one.
    pub diff: Option<String>,
    /// Artifacts, when there is no diff (or in addition to it).
    pub artifacts: Vec<ArtifactLine>,
    /// Test/command output captured in `orch.task_log`.
    pub test_output: Option<String>,
    /// Transcript summary produced by the summariser.
    pub transcript_summary: Option<String>,
    /// Per-task verdicts (run-level).
    pub task_verdicts: Vec<TaskVerdict>,
    /// Integration result (run-level).
    pub integration: Option<String>,
    /// Usage of the work being judged.
    pub usage: Usage,
}

impl Evidence {
    /// Evidence for a subject whose spec is `task_spec`.
    #[must_use]
    pub fn new(task_spec: impl Into<String>) -> Self {
        Self {
            task_spec: task_spec.into(),
            ..Self::default()
        }
    }

    /// Sets the acceptance criteria.
    #[must_use]
    pub fn with_acceptance_criteria(
        mut self,
        criteria: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.acceptance_criteria = criteria.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the diff.
    #[must_use]
    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }

    /// Sets the captured test/command output.
    #[must_use]
    pub fn with_test_output(mut self, output: impl Into<String>) -> Self {
        self.test_output = Some(output.into());
        self
    }

    /// Sets the transcript summary.
    #[must_use]
    pub fn with_transcript_summary(mut self, summary: impl Into<String>) -> Self {
        self.transcript_summary = Some(summary.into());
        self
    }

    /// Sets the artifacts.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: impl IntoIterator<Item = ArtifactLine>) -> Self {
        self.artifacts = artifacts.into_iter().collect();
        self
    }

    /// Sets the usage.
    #[must_use]
    pub const fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// The diff section: verbatim under [`DIFF_CAP`], summarised above it.
    #[must_use]
    pub fn diff_section(&self) -> Option<String> {
        let diff = self.diff.as_deref()?.trim();
        if diff.is_empty() {
            return None;
        }
        Some(if diff.chars().count() <= DIFF_CAP {
            diff.to_owned()
        } else {
            summarise_diff(diff)
        })
    }

    /// The test/command output section: the last [`TEST_OUTPUT_CAP`] characters.
    #[must_use]
    pub fn test_output_section(&self) -> Option<String> {
        let text = self.test_output.as_deref()?.trim();
        (!text.is_empty()).then(|| tail(text, TEST_OUTPUT_CAP))
    }

    /// The transcript summary section, capped at [`TRANSCRIPT_CAP`].
    #[must_use]
    pub fn transcript_section(&self) -> Option<String> {
        let text = self.transcript_summary.as_deref()?.trim();
        (!text.is_empty()).then(|| head(text, TRANSCRIPT_CAP))
    }

    /// The artifact list, one markdown bullet per artifact.
    #[must_use]
    pub fn artifacts_section(&self) -> Vec<String> {
        self.artifacts
            .iter()
            .map(|a| match &a.description {
                Some(description) => format!("- `{}` {} — {description}", a.kind, a.uri),
                None => format!("- `{}` {}", a.kind, a.uri),
            })
            .collect()
    }

    /// The per-task verdict list, one markdown bullet per task.
    #[must_use]
    pub fn verdicts_section(&self) -> Vec<String> {
        self.task_verdicts
            .iter()
            .map(|t| {
                let verdict = t.verdict.map_or("not evaluated", Verdict::as_str);
                match t.overall {
                    Some(overall) => {
                        format!("- {} ({}) — {verdict} ({overall:.2})", t.title, t.kind)
                    }
                    None => format!("- {} ({}) — {verdict}", t.title, t.kind),
                }
            })
            .collect()
    }

    /// The usage/cost line.
    #[must_use]
    pub fn usage_section(&self) -> String {
        let cost = self
            .usage
            .cost_usd
            .map_or_else(|| "unknown".to_owned(), |c: Decimal| format!("${c}"));
        format!(
            "- tokens: {} in / {} out (cache {} read, {} write)\n- cost: {cost}\n- wall: {} ms",
            self.usage.input_tokens,
            self.usage.output_tokens,
            self.usage.cache_read_tokens,
            self.usage.cache_write_tokens,
            self.usage.wall_ms,
        )
    }
}

/// The first `cap` characters of `text`, with an elision marker when it was cut.
#[must_use]
pub fn head(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let kept: String = text.chars().take(cap).collect();
    format!("{kept}\n… [truncated]")
}

/// The last `cap` characters of `text`, with an elision marker when it was cut.
#[must_use]
pub fn tail(text: &str, cap: usize) -> String {
    let count = text.chars().count();
    if count <= cap {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - cap).collect();
    format!("… [truncated]\n{kept}")
}

/// Summarises an oversized unified diff: a `git diff --stat`-style header, then
/// the first [`DIFF_FILE_LINES`] lines of each file's hunk.
#[must_use]
pub fn summarise_diff(diff: &str) -> String {
    let files = split_files(diff);
    let mut stat = Vec::with_capacity(files.len());
    let mut bodies = Vec::with_capacity(files.len());
    for (path, body) in &files {
        let added = body.lines().filter(|l| is_added(l)).count();
        let removed = body.lines().filter(|l| is_removed(l)).count();
        stat.push(format!(" {path} | {added} +, {removed} -"));
        let kept: Vec<&str> = body.lines().take(DIFF_FILE_LINES).collect();
        let elided = body.lines().count().saturating_sub(kept.len());
        let mut section = kept.join("\n");
        if elided > 0 {
            use std::fmt::Write as _;
            let _ = write!(section, "\n… [{elided} more lines in {path}]");
        }
        bodies.push(section);
    }
    format!(
        "diff --stat ({} files, truncated from {} characters)\n{}\n\n{}",
        files.len(),
        diff.chars().count(),
        stat.join("\n"),
        bodies.join("\n\n")
    )
}

/// `true` for an added line of a unified diff (`+++` headers excluded).
fn is_added(line: &str) -> bool {
    line.starts_with('+') && !line.starts_with("+++")
}

/// `true` for a removed line of a unified diff (`---` headers excluded).
fn is_removed(line: &str) -> bool {
    line.starts_with('-') && !line.starts_with("---")
}

/// Splits a unified diff into `(path, body)` per file.
fn split_files(diff: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some((path, body)) = current.take() {
                files.push((path, body.join("\n")));
            }
            current = Some((diff_path(rest), vec![line]));
        } else if let Some((_, body)) = current.as_mut() {
            body.push(line);
        }
    }
    if let Some((path, body)) = current.take() {
        files.push((path, body.join("\n")));
    }
    if files.is_empty() {
        files.push(("(unnamed)".to_owned(), diff.to_owned()));
    }
    files
}

/// `a/src/lib.rs b/src/lib.rs` → `src/lib.rs`.
fn diff_path(rest: &str) -> String {
    rest.split_whitespace().next_back().map_or_else(
        || rest.to_owned(),
        |p| p.strip_prefix("b/").unwrap_or(p).to_owned(),
    )
}

/// Removes every mention of the executor's route from the evidence
/// (`plan/06-memory-and-learning.md` §3.2, anti-gaming).
#[derive(Debug, Clone, Default)]
pub struct Scrubber {
    terms: Vec<String>,
}

impl Scrubber {
    /// A scrubber that hides `terms` (aliases, provider model ids, worker
    /// names). Terms shorter than three characters are ignored, so a scrubber
    /// never eats ordinary words.
    #[must_use]
    pub fn new(terms: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut terms: Vec<String> = terms
            .into_iter()
            .map(Into::into)
            .filter(|t| t.trim().chars().count() >= 3)
            .map(|t| t.trim().to_lowercase())
            .collect();
        terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
        terms.dedup();
        Self { terms }
    }

    /// Every term of a route: its alias, its provider model id and its worker.
    #[must_use]
    pub fn for_route(route: &Route, provider_model: Option<&str>) -> Self {
        let mut terms = vec![route.model.to_string(), route.worker.to_string()];
        terms.extend(provider_model.map(ToOwned::to_owned));
        Self::new(terms)
    }

    /// Adds more terms (e.g. every configured alias).
    #[must_use]
    pub fn with_terms(mut self, terms: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let extra = Self::new(terms);
        self.terms.extend(extra.terms);
        self.terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
        self.terms.dedup();
        self
    }

    /// `true` when nothing would be scrubbed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Replaces every case-insensitive occurrence of a term with [`REDACTED`],
    /// and drops `route`/`model`/`worker` key lines wholesale.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let without_keys: Vec<String> = text
            .lines()
            .map(|line| {
                if is_route_key_line(line) {
                    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                    format!("{indent}{REDACTED}")
                } else {
                    line.to_owned()
                }
            })
            .collect();
        let mut out = without_keys.join("\n");
        for term in &self.terms {
            out = replace_ignore_case(&out, term, REDACTED);
        }
        out
    }

    /// Scrubs an optional section.
    #[must_use]
    pub fn scrub_opt(&self, text: Option<String>) -> Option<String> {
        text.map(|t| self.scrub(&t))
    }

    /// Scrubs a list of lines.
    #[must_use]
    pub fn scrub_lines(&self, lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| self.scrub(l)).collect()
    }
}

/// `route: …`, `"model": …`, `- worker: …` — a key line naming the route.
fn is_route_key_line(line: &str) -> bool {
    let trimmed = line.trim().trim_start_matches(['-', '*', ' ']).trim();
    let key = trimmed
        .trim_start_matches('"')
        .split([':', '='])
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('"')
        .to_ascii_lowercase();
    matches!(key.as_str(), "route" | "model" | "worker" | "model_alias")
        && trimmed.contains([':', '='])
}

/// Case-insensitive `str::replace`.
fn replace_ignore_case(haystack: &str, needle_lower: &str, with: &str) -> String {
    if needle_lower.is_empty() {
        return haystack.to_owned();
    }
    let lower = haystack.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;
    while let Some(found) = lower[cursor..].find(needle_lower) {
        let start = cursor + found;
        let end = start + needle_lower.len();
        out.push_str(&haystack[cursor..start]);
        out.push_str(with);
        cursor = end;
    }
    out.push_str(&haystack[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevin_domain::{ModelAlias, WorkerKind};

    #[test]
    fn a_small_diff_is_passed_through_and_a_large_one_summarised() {
        let small = "diff --git a/a.rs b/a.rs\n+one\n-two";
        let evidence = Evidence::new("spec").with_diff(small);
        assert_eq!(evidence.diff_section().as_deref(), Some(small));

        let big = format!(
            "diff --git a/a.rs b/a.rs\n{}\ndiff --git a/b.rs b/b.rs\n+small change",
            "+x\n".repeat(DIFF_CAP)
        );
        let summarised = Evidence::new("spec")
            .with_diff(big)
            .diff_section()
            .expect("section");
        assert!(summarised.starts_with("diff --stat (2 files"));
        assert!(summarised.contains("a.rs |"));
        assert!(summarised.contains("more lines in a.rs"));
        assert!(summarised.contains("+small change"));
        assert!(summarised.chars().count() < DIFF_CAP);
    }

    #[test]
    fn logs_keep_their_tail_and_summaries_their_head() {
        let log = format!("{}END", "line\n".repeat(4000));
        let section = Evidence::new("s")
            .with_test_output(log)
            .test_output_section()
            .unwrap();
        assert!(section.starts_with("… [truncated]"));
        assert!(section.ends_with("END"));
        assert!(section.chars().count() <= TEST_OUTPUT_CAP + 16);

        let summary = "s".repeat(TRANSCRIPT_CAP * 2);
        let section = Evidence::new("s")
            .with_transcript_summary(summary)
            .transcript_section()
            .unwrap();
        assert!(section.ends_with("… [truncated]"));
    }

    #[test]
    fn the_scrubber_hides_the_route_everywhere() {
        let route = Route::new(WorkerKind::Claude, ModelAlias::new("opus5-claude").unwrap());
        let scrubber = Scrubber::for_route(&route, Some("claude-opus-5"));
        let text = "route: claude/opus5-claude\nThe agent OPUS5-CLAUDE used claude-opus-5.\nmodel = \"x\"\nkeep this";
        let scrubbed = scrubber.scrub(text);
        assert!(!scrubbed.to_lowercase().contains("opus5-claude"));
        assert!(!scrubbed.to_lowercase().contains("claude-opus-5"));
        assert!(!scrubbed.contains("model = "));
        assert!(scrubbed.contains("keep this"));
    }

    #[test]
    fn short_terms_never_scrub_ordinary_words() {
        let scrubber = Scrubber::new(["ai", "go"]);
        assert!(scrubber.is_empty());
        assert_eq!(scrubber.scrub("going ai"), "going ai");
    }
}
