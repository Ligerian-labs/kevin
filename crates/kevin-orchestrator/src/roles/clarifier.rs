//! The clarifier: it turns the questions Kevin decided to ask into questions a
//! human can answer, and it owns the pure question-selection rules of
//! `plan/05-orchestration.md` §3.2 ([`select_questions`]).

use kevin_domain::{ProposedQuestion, QuestionPolicy, RunMode, TaskKind, Understanding};
use serde::{Deserialize, Serialize};

use super::context::{ASSUMPTION_PREFIX, RoleContext, RoleLimits};
use super::{Role, RoleError, RoleRequest, build_request, deserialize, extract, schemas, vars_of};

const SYSTEM: &str = include_str!("../../prompts/clarifier.system.md");
const USER: &str = include_str!("../../prompts/clarifier.user.md");

/// The clarifier's output (`kevin.questions.v1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftedQuestions {
    /// The rewritten questions, in the order they must be shown.
    pub questions: Vec<ProposedQuestion>,
}

impl DraftedQuestions {
    /// Checks every question against the domain's own bounds.
    pub fn validate(&self) -> Result<(), kevin_domain::values::InvalidValue> {
        self.questions
            .iter()
            .try_for_each(ProposedQuestion::validate)
    }
}

/// Question drafting / refinement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clarifier;

impl Role for Clarifier {
    type Output = DraftedQuestions;

    fn name(&self) -> &'static str {
        "clarifier"
    }

    fn task_kind(&self) -> TaskKind {
        TaskKind::Clarify
    }

    fn build(&self, ctx: &RoleContext) -> RoleRequest {
        let mut vars = vars_of(ctx);
        let selection = ctx.understanding.as_ref().map(|u| {
            select_questions(
                u,
                ctx.run_mode.as_ref().unwrap_or(&RunMode::Interactive),
                &ctx.limits,
            )
        });
        if let Some(selection) = &selection {
            vars.set_lines(
                "questions",
                selection.asked.iter().map(SelectedQuestion::render),
            );
            vars.set_lines(
                "assumptions",
                selection.assumptions.iter().map(|a| format!("- {a}")),
            );
        }
        build_request(
            SYSTEM,
            USER,
            vars,
            schemas::questions().clone(),
            schemas::QUESTIONS_V1_ID,
        )
    }

    fn parse(&self, raw: &str) -> Result<DraftedQuestions, RoleError> {
        let role = self.name();
        let value = extract(role, raw, schemas::questions())?;
        let drafted: DraftedQuestions = deserialize(role, value)?;
        drafted.validate().map_err(|err| RoleError::Invalid {
            role,
            subject: "questions",
            message: err.to_string(),
        })?;
        Ok(drafted)
    }
}

// ---------------------------------------------------------------------------
// Question selection (plan/05-orchestration.md §3.2)
// ---------------------------------------------------------------------------

/// A question Kevin decided to ask, with the policy its run mode implies.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectedQuestion {
    /// The proposal as the planner wrote it.
    pub question: ProposedQuestion,
    /// When (and whether) an unanswered question falls back to the default.
    pub policy: QuestionPolicy,
}

impl SelectedQuestion {
    fn render(&self) -> String {
        let options = if self.question.options.is_empty() {
            "free text".to_owned()
        } else {
            self.question
                .options
                .iter()
                .map(|o| {
                    let star = if o.recommended { "*" } else { "" };
                    match &o.description {
                        Some(d) => format!("{}{star} ({d})", o.label),
                        None => format!("{}{star}", o.label),
                    }
                })
                .collect::<Vec<_>>()
                .join(" | ")
        };
        format!(
            "- [confidence if unasked {:.2}] {}\n  options: {}\n  why it matters: {}",
            self.question.confidence_if_unasked,
            self.question.text,
            options,
            self.question.why_it_matters
        )
    }
}

/// What the selection rules decided for one run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QuestionSelection {
    /// Questions to ask, lowest confidence first.
    pub asked: Vec<SelectedQuestion>,
    /// `Assumed: …` lines for every proposal that will not be asked; the saga
    /// appends them to [`Understanding::assumptions`].
    pub assumptions: Vec<String>,
}

/// Applies the selection rules of `plan/05-orchestration.md` §3.2:
///
/// - a proposal becomes a question only when `confidence_if_unasked <
///   orchestrator.question_confidence_threshold`, lowest confidence first, at
///   most `orchestrator.max_questions_per_run` of them;
/// - interactive runs block on every question; headless runs apply a
///   recommended option immediately and otherwise wait
///   `orchestrator.question_default_timeout`; Kohral runs never wait, so a
///   question without a recommended option is not asked at all;
/// - every dropped proposal becomes an `Assumed: …` assumption.
#[must_use]
pub fn select_questions(
    understanding: &Understanding,
    mode: &RunMode,
    limits: &RoleLimits,
) -> QuestionSelection {
    let candidates = understanding.questions_to_ask(
        limits.question_confidence_threshold,
        limits.max_questions_per_run,
    );

    let mut selection = QuestionSelection::default();
    for question in &understanding.proposed_questions {
        let selected = candidates.iter().any(|c| std::ptr::eq(*c, question));
        let recommended = question.recommended_option();
        let policy = match mode {
            RunMode::Interactive => Some(QuestionPolicy::Block),
            RunMode::Headless => Some(if recommended.is_some() {
                QuestionPolicy::IMMEDIATE_DEFAULT
            } else {
                QuestionPolicy::DefaultAfter {
                    timeout: limits.question_default_timeout,
                }
            }),
            // Kohral never waits: without a default there is nothing to apply,
            // so the planner proceeds on its best guess instead.
            RunMode::Kohral { .. } => recommended.map(|_| QuestionPolicy::IMMEDIATE_DEFAULT),
        };
        match (selected, policy) {
            (true, Some(policy)) => selection.asked.push(SelectedQuestion {
                question: question.clone(),
                policy,
            }),
            _ => selection.assumptions.push(assumption_for(question)),
        }
    }
    selection
}

/// `Assumed: <question> → <recommended option>` (or the planner's best guess).
fn assumption_for(question: &ProposedQuestion) -> String {
    match question.recommended_option() {
        Some(option) => format!("{ASSUMPTION_PREFIX}{} → {}", question.text, option.label),
        None => format!(
            "{ASSUMPTION_PREFIX}{} → planner's best guess (nobody could be asked)",
            question.text
        ),
    }
}

#[cfg(test)]
mod tests {
    use kevin_domain::{Complexity, QuestionOption};

    use super::*;

    fn question(text: &str, confidence: f32, recommended: bool) -> ProposedQuestion {
        ProposedQuestion {
            text: text.to_owned(),
            options: if recommended {
                vec![
                    QuestionOption::new("yes").recommended(),
                    QuestionOption::new("no"),
                ]
            } else {
                vec![]
            },
            multi_select: false,
            why_it_matters: "it matters".to_owned(),
            confidence_if_unasked: confidence,
        }
    }

    fn understanding(questions: Vec<ProposedQuestion>) -> Understanding {
        Understanding {
            proposed_questions: questions,
            complexity: Complexity::Low,
            ..Understanding::new("objective", "criterion")
        }
    }

    #[test]
    fn only_questions_below_the_threshold_are_asked() {
        let u = understanding(vec![
            question("low confidence", 0.2, true),
            question("high confidence", 0.95, true),
        ]);
        let selection = select_questions(&u, &RunMode::Interactive, &RoleLimits::default());
        assert_eq!(selection.asked.len(), 1);
        assert_eq!(selection.asked[0].question.text, "low confidence");
        assert_eq!(
            selection.assumptions,
            vec![format!("{ASSUMPTION_PREFIX}high confidence → yes")]
        );
    }

    #[test]
    fn kohral_never_asks_a_question_without_a_recommended_option() {
        let u = understanding(vec![question("open", 0.1, false)]);
        let mode = RunMode::Kohral {
            turn_id: "t".into(),
            session_key: "s".into(),
            session_id: "i".into(),
        };
        let selection = select_questions(&u, &mode, &RoleLimits::default());
        assert!(selection.asked.is_empty());
        assert!(selection.assumptions[0].ends_with("planner's best guess (nobody could be asked)"));
    }

    #[test]
    fn parses_fenced_questions_and_rejects_a_broken_one() {
        let raw = "```json\n{\"questions\":[{\"text\":\"q\",\"options\":[],\
                   \"why_it_matters\":\"w\",\"confidence_if_unasked\":0.1}]}\n```";
        let drafted = Clarifier.parse(raw).unwrap();
        assert_eq!(drafted.questions.len(), 1);

        let blank = "{\"questions\":[{\"text\":\"  \",\"options\":[],\
                     \"why_it_matters\":\"w\",\"confidence_if_unasked\":0.1}]}";
        let err = Clarifier.parse(blank).unwrap_err();
        assert!(
            matches!(
                err,
                RoleError::Invalid {
                    subject: "questions",
                    ..
                }
            ),
            "{err}"
        );
    }
}
