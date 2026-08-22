//! Terminal prompts for clarification questions and plan approval
//! (`plan/07-api-and-tui.md` §3: "questions in non-TUI mode are answered inline
//! on the terminal").
//!
//! Prompts are written to **stderr** so `--json` keeps a clean line protocol on
//! stdout, and answers are read from stdin, one line each. A closed stdin (EOF)
//! is not an error: the caller falls back to printing the `kevin answer` /
//! `kevin approve` hint and the run keeps waiting.

use std::io::BufRead as _;

use kevin_domain::{Answer, QuestionOption};

use crate::cmd::answer::actor;

/// What a human decided about a proposed plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Approve it as proposed.
    Approve,
    /// Reject it with feedback for the next planning call.
    Reject(String),
}

/// Line-oriented stdin prompts.
///
/// Reads happen on a blocking thread (`tokio`'s `io-std` feature is not
/// enabled workspace-wide) and the follower awaits them, so a prompt suspends
/// the event stream until the human answers or stdin closes.
#[derive(Debug, Default)]
pub struct Prompter {
    closed: bool,
}

impl Prompter {
    /// A prompter reading from the process' stdin.
    #[must_use]
    pub fn new() -> Self {
        Self { closed: false }
    }

    async fn read_line(&mut self) -> Option<String> {
        if self.closed {
            return None;
        }
        let line = tokio::task::spawn_blocking(|| {
            let mut buffer = String::new();
            match std::io::stdin().lock().read_line(&mut buffer) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(buffer),
            }
        })
        .await
        .ok()
        .flatten();
        if line.is_none() {
            self.closed = true;
        }
        line
    }

    /// Asks one clarification question; `None` means "no human available".
    pub async fn ask(
        &mut self,
        text: &str,
        options: &[QuestionOption],
        multi_select: bool,
        default: Option<&Answer>,
    ) -> Option<Answer> {
        eprintln!("\n? {text}");
        for (index, option) in options.iter().enumerate() {
            let marker = if option.recommended { " (recommended)" } else { "" };
            let description = option
                .description
                .as_ref()
                .map_or_else(String::new, |d| format!(" — {d}"));
            eprintln!("  {}) {}{description}{marker}", index + 1, option.label);
        }
        eprintln!(
            "  [{}, or free text; Enter = {}]",
            if multi_select {
                "numbers separated by commas"
            } else {
                "a number"
            },
            recommended(options).map_or("free text", |o| o.label.as_str()),
        );
        let raw = self.read_line().await?;
        parse_answer(&raw, options, multi_select).or_else(|| {
            default.map(|answer| Answer {
                selected: answer.selected.clone(),
                free_text: answer.free_text.clone(),
                answered_by: actor(),
            })
        })
    }

    /// Asks whether to approve the proposed plan; `None` means "no human".
    pub async fn approve_plan(&mut self, titles: &[String]) -> Option<Decision> {
        eprintln!("\nproposed plan ({} tasks):", titles.len());
        for (index, title) in titles.iter().enumerate() {
            eprintln!("  {}. {title}", index + 1);
        }
        eprintln!("  approve? [Y/n] (n asks for feedback)");
        let raw = self.read_line().await?;
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => Some(Decision::Approve),
            "n" | "no" => {
                eprintln!("  feedback for the planner:");
                let feedback = self.read_line().await?;
                let feedback = feedback.trim();
                Some(Decision::Reject(if feedback.is_empty() {
                    "rejected without feedback".to_owned()
                } else {
                    feedback.to_owned()
                }))
            }
            other => Some(Decision::Reject(other.to_owned())),
        }
    }
}

/// Interprets one answer line: indices, an exact label, or free text.
///
/// An empty line selects the recommended option when there is one; without a
/// recommendation an empty line means "no answer yet" (`None`).
fn parse_answer(raw: &str, options: &[QuestionOption], multi_select: bool) -> Option<Answer> {
    let trimmed = raw.trim();
    let by = actor();
    if trimmed.is_empty() {
        return recommended(options).map(|o| Answer::selected([o.label.clone()], by));
    }
    if let Some(selected) = by_index(trimmed, options, multi_select) {
        return Some(Answer::selected(selected, by));
    }
    if let Some(option) = options
        .iter()
        .find(|o| o.label.eq_ignore_ascii_case(trimmed))
    {
        return Some(Answer::selected([option.label.clone()], by));
    }
    Some(Answer {
        selected: Vec::new(),
        free_text: Some(trimmed.to_owned()),
        answered_by: by,
    })
}

fn by_index(raw: &str, options: &[QuestionOption], multi_select: bool) -> Option<Vec<String>> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.is_empty() || (!multi_select && parts.len() > 1) {
        return None;
    }
    let mut selected = Vec::with_capacity(parts.len());
    for part in parts {
        let index: usize = part.parse().ok()?;
        let option = options.get(index.checked_sub(1)?)?;
        selected.push(option.label.clone());
    }
    Some(selected)
}

fn recommended(options: &[QuestionOption]) -> Option<&QuestionOption> {
    options.iter().find(|o| o.recommended)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<QuestionOption> {
        vec![
            QuestionOption {
                label: "postgres".to_owned(),
                description: None,
                recommended: true,
            },
            QuestionOption {
                label: "sqlite".to_owned(),
                description: None,
                recommended: false,
            },
        ]
    }

    #[test]
    fn an_index_selects_the_option() {
        let answer = parse_answer("2", &options(), false).unwrap();
        assert_eq!(answer.selected, vec!["sqlite".to_owned()]);
        assert!(answer.free_text.is_none());
    }

    #[test]
    fn several_indices_need_multi_select() {
        assert!(parse_answer("1,2", &options(), false).unwrap().free_text.is_some());
        assert_eq!(
            parse_answer("1,2", &options(), true).unwrap().selected,
            vec!["postgres".to_owned(), "sqlite".to_owned()]
        );
    }

    #[test]
    fn a_label_matches_case_insensitively() {
        assert_eq!(
            parse_answer("SQLite", &options(), false).unwrap().selected,
            vec!["sqlite".to_owned()]
        );
    }

    #[test]
    fn anything_else_is_free_text() {
        let answer = parse_answer("use duckdb", &options(), false).unwrap();
        assert!(answer.selected.is_empty());
        assert_eq!(answer.free_text.as_deref(), Some("use duckdb"));
    }

    #[test]
    fn an_empty_line_takes_the_recommendation() {
        assert_eq!(
            parse_answer("", &options(), false).unwrap().selected,
            vec!["postgres".to_owned()]
        );
        let no_recommendation = vec![QuestionOption {
            label: "a".to_owned(),
            description: None,
            recommended: false,
        }];
        assert!(parse_answer("  ", &no_recommendation, false).is_none());
    }
}
