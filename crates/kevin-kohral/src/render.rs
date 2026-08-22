//! Rendering the run's events into the Markdown narrative Kohral shows
//! (`plan/08-kohral-runtime.md` §1.3).
//!
//! A Kohral conversation is a chat: the operator sees `partial_output` grow
//! while the turn runs and reads it as the agent thinking out loud. What Kevin
//! puts there is therefore a *narrative*, not a log — the understanding, the
//! assumptions it made instead of asking (`plan/08` §3), one line per task
//! transition, the integration result, and finally the answer.
//!
//! Every function returns `Option<String>`: `None` means "nothing worth
//! showing", and the projection then leaves `seq` alone. Every returned chunk
//! ends with a newline so appends never run together.

use std::fmt::Write as _;

use kevin_domain::task::TaskEvent;
use kevin_domain::{Answer, ArtifactRef, BudgetDimension, Plan, RunFailureReason, Understanding};
use rust_decimal::Decimal;

/// Longest single line the narrative shows before eliding.
const MAX_LINE: usize = 400;

/// The planner's understanding, plus the assumptions it recorded.
#[must_use]
pub fn understanding(understanding: &Understanding) -> Option<String> {
    let objective = understanding.objective.trim();
    let mut out = String::from("\n### Understanding\n\n");
    if objective.is_empty() {
        out.push_str("_(the planner returned no objective)_\n");
    } else {
        let _ = writeln!(out, "{}", excerpt(objective));
    }
    if !understanding.assumptions.is_empty() {
        out.push_str("\n### Assumptions I made\n\n");
        for assumption in &understanding.assumptions {
            let _ = writeln!(out, "- {}", excerpt(assumption.trim()));
        }
    }
    Some(out)
}

/// One question answered by its default, i.e. an assumption the operator may
/// want to correct in the next turn.
#[must_use]
pub fn assumption(question: &str, answer: &Answer) -> Option<String> {
    let chosen = if answer.selected.is_empty() {
        answer.free_text.clone().unwrap_or_default()
    } else {
        answer.selected.join(", ")
    };
    if chosen.trim().is_empty() {
        return None;
    }
    Some(format!(
        "\n- **Assumed** (not asked): {} → {}\n",
        excerpt(question.trim()),
        excerpt(chosen.trim())
    ))
}

/// The approved plan, as a numbered list.
#[must_use]
pub fn plan(plan: &Plan) -> Option<String> {
    if plan.tasks.is_empty() {
        return None;
    }
    let mut out = String::from("\n### Plan\n\n");
    for (index, task) in plan.tasks.iter().enumerate() {
        let _ = writeln!(out, "{}. {}", index + 1, excerpt(task.title.trim()));
    }
    Some(out)
}

/// Execution started with `tasks` tasks.
#[must_use]
pub fn execution_started(tasks: usize) -> Option<String> {
    if tasks == 0 {
        return None;
    }
    let plural = if tasks == 1 { "task" } else { "tasks" };
    Some(format!("\n_Working on {tasks} {plural}…_\n"))
}

/// One task transition.
#[must_use]
pub fn task_line(title: &str, event: &TaskEvent) -> Option<String> {
    let title = excerpt(title.trim());
    let line = match event {
        TaskEvent::Progressed { summary, .. } => {
            let summary = summary.trim();
            if summary.is_empty() {
                return None;
            }
            format!("- {title}: {}", excerpt(summary))
        }
        TaskEvent::AttemptSucceeded { summary, .. } => {
            let summary = summary.trim();
            if summary.is_empty() {
                format!("- done — {title}")
            } else {
                format!("- done — {title}: {}", excerpt(summary))
            }
        }
        TaskEvent::AttemptFailed { class, message, .. } => format!(
            "- failed — {title} ({}): {}",
            class.as_str(),
            excerpt(message.trim())
        ),
        TaskEvent::Retried {
            next_attempt_no,
            reason,
        } => format!(
            "- retrying — {title} (attempt {next_attempt_no}): {}",
            excerpt(reason.trim())
        ),
        TaskEvent::Skipped { reason } => {
            format!("- skipped — {title}: {}", excerpt(reason.trim()))
        }
        _ => return None,
    };
    Some(format!("{line}\n"))
}

/// A budget dimension ran out.
#[must_use]
pub fn budget_exhausted(
    dimension: BudgetDimension,
    limit: Decimal,
    actual: Decimal,
) -> Option<String> {
    Some(format!(
        "\n_Budget exhausted: {} reached {actual} of {limit}._\n",
        dimension.as_str()
    ))
}

/// The integration result and the artifacts it produced.
#[must_use]
pub fn integration(summary: &str, artifacts: &[ArtifactRef]) -> Option<String> {
    let summary = summary.trim();
    if summary.is_empty() && artifacts.is_empty() {
        return None;
    }
    let mut out = String::from("\n### Result\n\n");
    if !summary.is_empty() {
        let _ = writeln!(out, "{}", excerpt(summary));
    }
    for artifact in artifacts {
        let _ = writeln!(out, "- {:?}: {}", artifact.kind, artifact.uri);
    }
    Some(out)
}

/// The run failed.
#[must_use]
pub fn failure(reason: &RunFailureReason, message: &str) -> Option<String> {
    let message = message.trim();
    let mut out = format!("\n### Failed ({})\n", reason.as_str());
    if !message.is_empty() {
        let _ = writeln!(out, "\n{}", excerpt(message));
    }
    Some(out)
}

/// The turn was stopped.
#[must_use]
pub fn cancellation(by: &str, reason: &str) -> Option<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Some(format!("\n_Stopped by {by}._\n"));
    }
    Some(format!("\n_Stopped by {by}: {}._\n", excerpt(reason)))
}

/// Cuts a line that would swamp a chat bubble.
fn excerpt(text: &str) -> String {
    let single_line = text.replace(['\r', '\n'], " ");
    if single_line.chars().count() <= MAX_LINE {
        return single_line;
    }
    let kept: String = single_line.chars().take(MAX_LINE - 1).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use kevin_domain::task::TaskEvent;
    use kevin_domain::{Answer, AttemptId, FailureClass, RunFailureReason, Usage};

    use super::{assumption, excerpt, failure, task_line, understanding};

    #[test]
    fn the_understanding_and_its_assumptions_are_rendered() {
        let mut u =
            kevin_domain::Understanding::new("Add a health endpoint", "the endpoint answers 200");
        u.assumptions = vec!["Assumed: axum".to_owned()];
        let text = understanding(&u).expect("rendered");
        assert!(text.contains("### Understanding"));
        assert!(text.contains("Add a health endpoint"));
        assert!(text.contains("### Assumptions I made"));
        assert!(text.contains("- Assumed: axum"));
        assert!(text.ends_with('\n'), "every chunk ends with a newline");
    }

    #[test]
    fn a_defaulted_answer_becomes_a_visible_assumption() {
        let answer = Answer::selected(["Use axum".to_owned()], Answer::DEFAULT_ANSWERED_BY);
        let text = assumption("Which framework?", &answer).expect("rendered");
        assert!(text.contains("**Assumed** (not asked): Which framework? → Use axum"));

        let empty = Answer::selected(Vec::<String>::new(), Answer::DEFAULT_ANSWERED_BY);
        assert!(assumption("Which framework?", &empty).is_none());
    }

    #[test]
    fn task_transitions_render_one_line_each() {
        let attempt_id = AttemptId::new();
        let succeeded = TaskEvent::AttemptSucceeded {
            attempt_id,
            artifacts: Vec::new(),
            summary: "wrote the handler".to_owned(),
            usage: Usage::ZERO,
        };
        let line = task_line("Implement /health", &succeeded).expect("rendered");
        assert_eq!(line, "- done — Implement /health: wrote the handler\n");

        let failed = TaskEvent::AttemptFailed {
            attempt_id,
            class: FailureClass::Transient,
            message: "429".to_owned(),
            usage: Usage::ZERO,
            retry_possible: true,
        };
        assert_eq!(
            task_line("Implement /health", &failed).expect("rendered"),
            "- failed — Implement /health (transient): 429\n"
        );

        let progressed = TaskEvent::Progressed {
            attempt_id,
            summary: String::new(),
            usage_delta: Usage::ZERO,
            log_seq: 1,
        };
        assert!(
            task_line("t", &progressed).is_none(),
            "an empty progress summary adds nothing, so `seq` does not move"
        );
    }

    #[test]
    fn a_failure_names_its_reason() {
        let text = failure(&RunFailureReason::BudgetExhausted, "over $10").expect("rendered");
        assert!(text.contains("### Failed (budget_exhausted)"));
        assert!(text.contains("over $10"));
    }

    #[test]
    fn long_lines_are_elided_and_flattened() {
        let text = excerpt(&"x".repeat(1000));
        assert_eq!(text.chars().count(), 400);
        assert!(text.ends_with('…'));
        assert_eq!(excerpt("a\nb"), "a b");
    }
}
