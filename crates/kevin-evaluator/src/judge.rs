//! The judge role: prompts, schema, parsing (`plan/06-memory-and-learning.md`
//! §3.2).
//!
//! [`Judge`] is pure — it turns a [`JudgeContext`] into a [`JudgeRequest`] and
//! parses the worker's answer into a [`JudgeOutput`]. Running it against a
//! worker is [`crate::runner::JudgeRunner`]'s job.
//!
//! The judge never sees a route: [`JudgeContext::scrubber`] is applied to every
//! section before rendering.

use kevin_worker::structured::StructuredError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::evidence::{Evidence, Scrubber};
use crate::prompt::{Vars, render};
use crate::rubric::Rubric;
use crate::schemas;

/// The prompt-injection rule the judge's system prompt states verbatim
/// (`plan/09-security.md`). Kept in step with the orchestrator's copy by
/// `ac_ws19_*`-adjacent unit tests in both crates.
pub const PROMPT_INJECTION_RULE: &str = include_str!("../prompts/injection_rule.md");

const COMMON_RULES: &str = include_str!("../prompts/common_rules.md");
const SYSTEM: &str = include_str!("../prompts/judge.system.md");
const USER: &str = include_str!("../prompts/judge.user.md");

/// What the judge is asked to score.
#[derive(Debug, Clone)]
pub struct JudgeContext {
    /// The rubric in force.
    pub rubric: Rubric,
    /// The evidence, before scrubbing.
    pub evidence: Evidence,
    /// Removes every route mention from the evidence.
    pub scrubber: Scrubber,
}

impl JudgeContext {
    /// A context with an empty scrubber (nothing to hide).
    #[must_use]
    pub fn new(rubric: Rubric, evidence: Evidence) -> Self {
        Self {
            rubric,
            evidence,
            scrubber: Scrubber::default(),
        }
    }

    /// Sets the scrubber.
    #[must_use]
    pub fn with_scrubber(mut self, scrubber: Scrubber) -> Self {
        self.scrubber = scrubber;
        self
    }

    /// The template variables, every section scrubbed and capped.
    #[must_use]
    fn vars(&self) -> Vars {
        let s = &self.scrubber;
        let e = &self.evidence;
        let mut vars = Vars::new();
        vars.set("rubric_id", &self.rubric.id)
            .set("criteria", self.rubric.as_prompt_block())
            .set("task_spec", s.scrub(&e.task_spec))
            .set_lines(
                "acceptance_criteria",
                s.scrub_lines(&bullets(&e.acceptance_criteria)),
            )
            .set_lines(
                "success_criteria",
                s.scrub_lines(&bullets(&e.success_criteria)),
            )
            .set_opt("plan", s.scrub_opt(e.plan.clone()))
            .set_opt("diff", s.scrub_opt(e.diff_section()))
            .set_lines("artifacts", s.scrub_lines(&e.artifacts_section()))
            .set_opt("test_output", s.scrub_opt(e.test_output_section()))
            .set_opt("transcript_summary", s.scrub_opt(e.transcript_section()))
            .set_lines("task_verdicts", s.scrub_lines(&e.verdicts_section()))
            .set_opt("integration", s.scrub_opt(e.integration.clone()))
            .set("usage", e.usage_section());
        vars
    }
}

/// `- item` per entry.
fn bullets(items: &[String]) -> Vec<String> {
    items.iter().map(|i| format!("- {i}")).collect()
}

/// What the judge asks a worker to do.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeRequest {
    /// Appended to the worker's system prompt.
    pub system: String,
    /// The message sent to the worker.
    pub user: String,
    /// The rubric-specialised `kevin.evaluation.v1` schema.
    pub schema: Value,
}

/// One criterion score as the judge reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeScore {
    /// Criterion key from the rubric.
    pub criterion: String,
    /// 0..=10.
    pub score: u8,
    /// Why, ≤ 400 characters.
    pub rationale: String,
}

/// One proposal as the judge drafted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeProposal {
    /// What it changes.
    pub kind: kevin_domain::ProposalKind,
    /// The proposed change.
    pub body: String,
    /// Why.
    pub rationale: String,
}

/// The judge's answer (`kevin.evaluation.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeOutput {
    /// Per-criterion scores.
    pub scores: Vec<JudgeScore>,
    /// The judge's own overall — logged, never trusted
    /// (`plan/06-memory-and-learning.md` §3.2).
    pub overall: f32,
    /// The judge's verdict; reconciled with the recomputed score.
    pub verdict: kevin_domain::Verdict,
    /// Lessons, ≤ 5.
    pub lessons: Vec<String>,
    /// Proposals, ≤ 3.
    pub proposals: Vec<JudgeProposal>,
}

impl JudgeOutput {
    /// `(criterion, score)` pairs for [`Rubric::overall`].
    #[must_use]
    pub fn score_pairs(&self) -> Vec<(String, u8)> {
        self.scores
            .iter()
            .map(|s| (s.criterion.clone(), s.score))
            .collect()
    }
}

/// The judge role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Judge;

impl Judge {
    /// Stable role name, used in logs and transcripts.
    pub const NAME: &'static str = "judge";

    /// Renders the prompts and specialises the schema for the rubric.
    #[must_use]
    pub fn build(self, ctx: &JudgeContext) -> JudgeRequest {
        let mut vars = ctx.vars();
        vars.set("injection_rule", PROMPT_INJECTION_RULE.trim())
            .set("schema_id", schemas::EVALUATION_V1_ID);
        let common = render(COMMON_RULES, &vars);
        vars.set("common_rules", common);
        JudgeRequest {
            system: render(SYSTEM, &vars),
            user: render(USER, &vars),
            schema: schemas::evaluation_for(&ctx.rubric),
        }
    }

    /// Parses a worker answer against the rubric-specialised schema and checks
    /// that every criterion was scored exactly once.
    pub fn parse(self, raw: &str, rubric: &Rubric) -> Result<JudgeOutput, JudgeOutputError> {
        let schema = schemas::evaluation_for(rubric);
        let value = kevin_worker::structured::extract_and_validate(raw, &schema)
            .map_err(JudgeOutputError::Structured)?;
        let output: JudgeOutput =
            serde_json::from_value(value).map_err(|e| JudgeOutputError::Deserialize {
                message: e.to_string(),
            })?;
        check_criteria(&output, rubric)?;
        Ok(output)
    }
}

/// Every rubric criterion scored exactly once.
fn check_criteria(output: &JudgeOutput, rubric: &Rubric) -> Result<(), JudgeOutputError> {
    let mut missing = Vec::new();
    for key in rubric.keys() {
        let count = output.scores.iter().filter(|s| s.criterion == key).count();
        if count != 1 {
            missing.push(format!("`{key}` scored {count} times, expected once"));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(JudgeOutputError::Structured(
            StructuredError::SchemaViolation { errors: missing },
        ))
    }
}

/// Why a judge answer could not be used.
#[derive(Debug, thiserror::Error)]
pub enum JudgeOutputError {
    /// No JSON, malformed JSON, or JSON that violates the schema.
    #[error("judge output: {0}")]
    Structured(#[source] StructuredError),
    /// Schema-valid JSON that does not deserialise.
    #[error("judge output does not deserialise: {message}")]
    Deserialize {
        /// Why.
        message: String,
    },
}

impl JudgeOutputError {
    /// `true` when the answer broke the schema — the one case the runner
    /// repairs with a follow-up turn.
    #[must_use]
    pub fn is_schema_violation(&self) -> bool {
        matches!(self, JudgeOutputError::Structured(e) if e.is_schema_violation())
    }

    /// The structured-output error behind this one, if any.
    #[must_use]
    pub const fn structured(&self) -> Option<&StructuredError> {
        match self {
            JudgeOutputError::Structured(e) => Some(e),
            JudgeOutputError::Deserialize { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::ArtifactLine;
    use kevin_domain::{ModelAlias, Route, Verdict, WorkerKind};

    fn ctx() -> JudgeContext {
        let evidence = Evidence::new("Add a /healthz endpoint")
            .with_acceptance_criteria(["returns 200", "has a test"])
            .with_diff("diff --git a/src/lib.rs b/src/lib.rs\n+fn healthz() {}")
            .with_test_output("running 1 test\ntest healthz ... ok")
            .with_artifacts([ArtifactLine {
                kind: "pr".to_owned(),
                uri: "https://example.invalid/pr/1".to_owned(),
                description: None,
            }]);
        JudgeContext::new(Rubric::builtin("code").unwrap(), evidence)
    }

    #[test]
    fn the_system_prompt_carries_the_rules_and_the_rubric() {
        let request = Judge.build(&ctx());
        assert!(request.system.contains(PROMPT_INJECTION_RULE.trim()));
        assert!(request.system.contains("`code`"));
        assert!(request.system.contains("`test_coverage` (weight 0.15)"));
        assert!(request.system.contains(schemas::EVALUATION_V1_ID));
        assert!(!request.system.contains("{{"), "{}", request.system);
        assert!(!request.user.contains("{{"), "{}", request.user);
        assert!(request.user.contains("Add a /healthz endpoint"));
        assert!(request.user.contains("- returns 200"));
        assert!(request.user.contains("# Diff"));
        assert!(request.user.contains("# Test and command output (tail)"));
    }

    #[test]
    fn empty_sections_are_dropped_from_the_user_prompt() {
        let ctx = JudgeContext::new(Rubric::builtin("default").unwrap(), Evidence::new("goal"));
        let request = Judge.build(&ctx);
        assert!(!request.user.contains("# Diff"));
        assert!(!request.user.contains("# Acceptance criteria"));
        assert!(request.user.contains("# Usage and cost"));
    }

    #[test]
    fn the_route_never_reaches_the_prompt() {
        let route = Route::new(WorkerKind::Codex, ModelAlias::new("gpt56-codex").unwrap());
        let mut c = ctx();
        c.evidence.task_spec = "Implemented by gpt56-codex on codex".to_owned();
        let c = c.with_scrubber(Scrubber::for_route(&route, Some("gpt-5.6")));
        let request = Judge.build(&c);
        assert!(!request.user.to_lowercase().contains("gpt56-codex"));
        assert!(!request.user.to_lowercase().contains("gpt-5.6"));
    }

    #[test]
    fn a_fenced_answer_parses_and_a_missing_criterion_is_a_schema_violation() {
        let rubric = Rubric::builtin("default").unwrap();
        let scores: Vec<String> = rubric
            .keys()
            .into_iter()
            .map(|k| format!("{{\"criterion\":\"{k}\",\"score\":8,\"rationale\":\"ok\"}}"))
            .collect();
        let raw = format!(
            "Here you go:\n```json\n{{\"scores\":[{}],\"overall\":0.8,\"verdict\":\"accept\",\"lessons\":[],\"proposals\":[]}}\n```",
            scores.join(",")
        );
        let output = Judge.parse(&raw, &rubric).unwrap();
        assert_eq!(output.verdict, Verdict::Accept);
        assert_eq!(output.scores.len(), 5);

        let short = format!(
            "{{\"scores\":[{}],\"overall\":0.8,\"verdict\":\"accept\",\"lessons\":[],\"proposals\":[]}}",
            scores[..4].join(",")
        );
        assert!(
            Judge
                .parse(&short, &rubric)
                .unwrap_err()
                .is_schema_violation()
        );
    }

    #[test]
    fn an_invented_criterion_is_rejected() {
        let rubric = Rubric::builtin("default").unwrap();
        let raw = "{\"scores\":[{\"criterion\":\"vibes\",\"score\":10,\"rationale\":\"nice\"}],\
                   \"overall\":1.0,\"verdict\":\"accept\",\"lessons\":[],\"proposals\":[]}";
        assert!(Judge.parse(raw, &rubric).unwrap_err().is_schema_violation());
    }
}
