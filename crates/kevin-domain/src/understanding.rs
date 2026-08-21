//! [`Understanding`] — the planner's structured comprehension of a goal,
//! serialised exactly as the `kevin.understanding.v1` JSON schema in
//! `plan/05-orchestration.md` §3.2.

use serde::{Deserialize, Serialize};

use crate::kinds::Complexity;
use crate::values::{InvalidValue, QuestionOption};

/// `$id` of the JSON schema this type mirrors.
pub const UNDERSTANDING_SCHEMA_ID: &str = "kevin.understanding.v1";

/// Maximum number of proposed questions (`maxItems`).
pub const MAX_PROPOSED_QUESTIONS: usize = 10;

/// Maximum options per proposed question (`maxItems`).
pub const MAX_QUESTION_OPTIONS: usize = 4;

/// Maximum length of `objective` (`maxLength`).
pub const MAX_OBJECTIVE_CHARS: usize = 2000;

/// The planner's understanding of a goal (`kevin.understanding.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Understanding {
    /// Restated objective (≤ 2000 chars).
    pub objective: String,
    /// Assumptions the planner made (includes "Assumed: …" lines for dropped questions).
    pub assumptions: Vec<String>,
    /// Risks identified.
    pub risks: Vec<String>,
    /// What success looks like (≥ 1).
    pub success_criteria: Vec<String>,
    /// Questions the planner would like answered (≤ 10).
    pub proposed_questions: Vec<ProposedQuestion>,
    /// Estimated complexity.
    pub complexity: Complexity,
    /// Task kinds the planner expects to use (free strings, validated later).
    pub suggested_task_kinds: Vec<String>,
    /// Files/URLs/memory ids the planner relied on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_refs: Vec<String>,
}

/// A clarification the planner proposes to ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedQuestion {
    /// Question text.
    pub text: String,
    /// Options (≤ 4); empty means free text.
    pub options: Vec<QuestionOption>,
    /// Several options may be selected.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub multi_select: bool,
    /// Why the answer matters.
    pub why_it_matters: String,
    /// Planner's confidence (0..=1) that it can proceed without asking.
    pub confidence_if_unasked: f32,
}

impl ProposedQuestion {
    /// The recommended option, if any.
    #[must_use]
    pub fn recommended_option(&self) -> Option<&QuestionOption> {
        self.options.iter().find(|o| o.recommended)
    }

    /// Whether the question should be asked given a confidence threshold
    /// (`orchestrator.question_confidence_threshold`).
    #[must_use]
    pub fn should_ask(&self, threshold: f32) -> bool {
        self.confidence_if_unasked < threshold
    }

    /// Checks the schema bounds (options ≤ 4, confidence in 0..=1, text non-empty).
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.text.trim().is_empty() {
            return Err(InvalidValue::new(
                "proposed_questions[].text",
                "must not be empty",
            ));
        }
        if self.options.len() > MAX_QUESTION_OPTIONS {
            return Err(InvalidValue::new(
                "proposed_questions[].options",
                format!("at most {MAX_QUESTION_OPTIONS} options"),
            ));
        }
        if !(0.0..=1.0).contains(&self.confidence_if_unasked) {
            return Err(InvalidValue::new(
                "proposed_questions[].confidence_if_unasked",
                "must be within 0..=1",
            ));
        }
        Ok(())
    }
}

impl Understanding {
    /// Minimal understanding with one success criterion and no questions.
    #[must_use]
    pub fn new(objective: impl Into<String>, success_criterion: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
            assumptions: Vec::new(),
            risks: Vec::new(),
            success_criteria: vec![success_criterion.into()],
            proposed_questions: Vec::new(),
            complexity: Complexity::Medium,
            suggested_task_kinds: Vec::new(),
            context_refs: Vec::new(),
        }
    }

    /// Checks the schema constraints serde cannot express: non-empty
    /// objective within 2000 chars, ≥ 1 success criterion, ≤ 10 proposed
    /// questions each valid.
    pub fn validate(&self) -> Result<(), InvalidValue> {
        if self.objective.trim().is_empty() {
            return Err(InvalidValue::new("objective", "must not be empty"));
        }
        if self.objective.chars().count() > MAX_OBJECTIVE_CHARS {
            return Err(InvalidValue::new(
                "objective",
                format!("longer than {MAX_OBJECTIVE_CHARS} characters"),
            ));
        }
        if self.success_criteria.is_empty() {
            return Err(InvalidValue::new(
                "success_criteria",
                "at least one criterion is required",
            ));
        }
        if self.proposed_questions.len() > MAX_PROPOSED_QUESTIONS {
            return Err(InvalidValue::new(
                "proposed_questions",
                format!("at most {MAX_PROPOSED_QUESTIONS} questions"),
            ));
        }
        self.proposed_questions
            .iter()
            .try_for_each(ProposedQuestion::validate)
    }

    /// Proposed questions worth asking, lowest confidence first, capped at
    /// `max_questions` (`plan/05-orchestration.md` §3.2 selection rules).
    #[must_use]
    pub fn questions_to_ask(&self, threshold: f32, max_questions: usize) -> Vec<&ProposedQuestion> {
        let mut asked: Vec<&ProposedQuestion> = self
            .proposed_questions
            .iter()
            .filter(|q| q.should_ask(threshold))
            .collect();
        asked.sort_by(|a, b| a.confidence_if_unasked.total_cmp(&b.confidence_if_unasked));
        asked.truncate(max_questions);
        asked
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_json() -> serde_json::Value {
        json!({
            "objective": "Add a /healthz endpoint",
            "assumptions": ["axum is already used"],
            "risks": ["none"],
            "success_criteria": ["GET /healthz returns 200"],
            "proposed_questions": [{
                "text": "Should it check the database?",
                "options": [
                    {"label": "yes", "description": "ping pg", "recommended": true},
                    {"label": "no"}
                ],
                "why_it_matters": "decides dependencies",
                "confidence_if_unasked": 0.25
            }],
            "complexity": "low",
            "suggested_task_kinds": ["implement", "test"],
            "context_refs": ["src/main.rs"]
        })
    }

    #[test]
    fn round_trips_schema_shape() {
        let u: Understanding = serde_json::from_value(sample_json()).unwrap();
        assert_eq!(u.complexity, Complexity::Low);
        assert_eq!(u.proposed_questions[0].options[1].label, "no");
        assert!(!u.proposed_questions[0].multi_select);
        let mut expected = sample_json();
        expected["proposed_questions"][0]["options"][1]["recommended"] = json!(false);
        assert_eq!(serde_json::to_value(&u).unwrap(), expected);
        let back: Understanding =
            serde_json::from_value(serde_json::to_value(&u).unwrap()).unwrap();
        assert_eq!(back, u);
        u.validate().unwrap();
    }

    #[test]
    fn rejects_unknown_fields_and_missing_required() {
        let mut v = sample_json();
        v["extra"] = json!(1);
        assert!(serde_json::from_value::<Understanding>(v).is_err());
        let mut v = sample_json();
        v.as_object_mut().unwrap().remove("complexity");
        assert!(serde_json::from_value::<Understanding>(v).is_err());
        let mut v = sample_json();
        v["proposed_questions"][0]["options"][0]["weird"] = json!(true);
        // QuestionOption is shared with the domain and tolerant of extra keys? No: schema
        // does not set additionalProperties on options, so extra keys are allowed.
        assert!(serde_json::from_value::<Understanding>(v).is_ok());
    }

    #[test]
    fn validate_checks_bounds() {
        let mut u = Understanding::new("x", "done");
        u.validate().unwrap();
        u.success_criteria.clear();
        assert!(u.validate().is_err());
        let mut u = Understanding::new("x", "done");
        u.objective = "a".repeat(2001);
        assert!(u.validate().is_err());
        let mut u = Understanding::new("x", "done");
        u.proposed_questions.push(ProposedQuestion {
            text: "q".into(),
            options: vec![QuestionOption::new("a"); 5],
            multi_select: false,
            why_it_matters: "w".into(),
            confidence_if_unasked: 0.1,
        });
        assert!(u.validate().is_err());
        u.proposed_questions[0].options.truncate(4);
        u.proposed_questions[0].confidence_if_unasked = 1.5;
        assert!(u.validate().is_err());
    }

    #[test]
    fn questions_to_ask_filters_sorts_and_caps() {
        let mut u = Understanding::new("x", "done");
        for (i, c) in [0.9f32, 0.3, 0.6, 0.1, 0.5].into_iter().enumerate() {
            u.proposed_questions.push(ProposedQuestion {
                text: format!("q{i}"),
                options: vec![],
                multi_select: false,
                why_it_matters: String::new(),
                confidence_if_unasked: c,
            });
        }
        let asked: Vec<_> = u
            .questions_to_ask(0.7, 3)
            .into_iter()
            .map(|q| q.text.clone())
            .collect();
        assert_eq!(asked, vec!["q3", "q1", "q4"]);
    }
}
