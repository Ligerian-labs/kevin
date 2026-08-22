//! The question inbox (`plan/07-api-and-tui.md` §Screens).
//!
//! Left: every open question across every run. Right: the selected question
//! with its options, the recommended marker, the default and the deadline
//! countdown of a `default_after` policy.

use kevin_api::dto::{QuestionDto, QuestionPolicyKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::fmt;
use crate::model::{InboxFocus, Model};
use crate::view::{cursor, empty, hint, list, pane, row_style};

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);
    questions(model, frame, columns[0]);
    detail(model, frame, columns[1]);
}

fn questions(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let focused = model.inbox.focus == InboxFocus::Questions;
    let width = usize::from(area.width).saturating_sub(13);
    let lines: Vec<Line<'static>> = if model.inbox.items.is_empty() {
        empty(model, "  nothing to answer")
    } else {
        model
            .inbox
            .items
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let selected = index == model.inbox.selected;
                Line::from(Span::styled(
                    format!(
                        "{}{} {}",
                        cursor(selected),
                        fmt::short_id(question.run_id.as_uuid()),
                        fmt::truncate(&question.text, width)
                    ),
                    row_style(model, focused && selected, "awaiting_input"),
                ))
            })
            .collect()
    };
    let block = pane(
        model,
        format!("Open ({})", model.inbox.items.len()),
        focused,
    )
    .title_bottom(hint(model, " j/k question · Tab options "));
    list(frame, area, block, None, lines, model.inbox.selected);
}

fn detail(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let focused = model.inbox.focus == InboxFocus::Options;
    let block = pane(model, "Answer", focused).title_bottom(hint(
        model,
        " Space pick · Enter submit · t text · Esc back ",
    ));
    let Some(question) = model.inbox.selected() else {
        frame.render_widget(
            Paragraph::new(empty(model, "  no question selected")).block(block),
            area,
        );
        return;
    };

    let width = usize::from(area.width).saturating_sub(4);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "run {} · task {}",
                fmt::short_id(question.run_id.as_uuid()),
                question
                    .task_id
                    .map_or_else(|| fmt::UNKNOWN.to_owned(), |id| fmt::short_id(id.as_uuid()))
            ),
            model.theme.dim(),
        )),
        Line::from(Span::styled(
            fmt::truncate(&question.text, width),
            model.theme.heading(),
        )),
        Line::from(""),
    ];

    if question.options.is_empty() {
        lines.push(Line::from(Span::styled(
            "  free text only — press `t`".to_owned(),
            model.theme.dim(),
        )));
    }
    for (index, option) in question.options.iter().enumerate() {
        let ticked = model.inbox.chosen.contains(&option.label);
        let marker = match (question.multi_select, ticked) {
            (true, true) => "[x]",
            (true, false) => "[ ]",
            (false, true) => "(•)",
            (false, false) => "( )",
        };
        let selected = focused && index == model.inbox.option_selected;
        let recommended = if option.recommended {
            " ★ recommended"
        } else {
            ""
        };
        let description = option
            .description
            .as_ref()
            .map(|text| format!(" — {text}"))
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            fmt::truncate(
                &format!(
                    "{}{marker} {}{recommended}{description}",
                    cursor(selected),
                    option.label
                ),
                width,
            ),
            row_style(model, selected, "awaiting_input"),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("free text: ", model.theme.dim()),
        Span::raw(
            model
                .inbox
                .free_text
                .clone()
                .unwrap_or_else(|| "(none — press t)".to_owned()),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        policy_line(model, question),
        model.theme.dim(),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

fn policy_line(model: &Model, question: &QuestionDto) -> String {
    let default = question.default.as_ref().map_or_else(
        || fmt::UNKNOWN.to_owned(),
        |answer| {
            let mut text = answer.selected.join(", ");
            if let Some(free) = answer.free_text.as_ref() {
                if !text.is_empty() {
                    text.push_str(" / ");
                }
                text.push_str(free);
            }
            text
        },
    );
    match question.policy.kind {
        QuestionPolicyKind::Block => format!("default: {default} · policy: blocks until answered"),
        QuestionPolicyKind::DefaultAfter => {
            let deadline = question.policy.timeout_ms.map(|timeout| {
                let elapsed = model
                    .now
                    .signed_duration_since(question.asked_at)
                    .num_milliseconds()
                    .max(0);
                let remaining = i64::try_from(timeout)
                    .unwrap_or(i64::MAX)
                    .saturating_sub(elapsed);
                if remaining <= 0 {
                    "expired".to_owned()
                } else {
                    format!("in {}", fmt::duration(remaining))
                }
            });
            format!(
                "default: {default} · applies {}",
                deadline.unwrap_or_else(|| fmt::UNKNOWN.to_owned())
            )
        }
    }
}
