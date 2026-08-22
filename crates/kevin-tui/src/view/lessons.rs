//! Lessons & proposals — two tabs (`plan/07-api-and-tui.md` §Screens).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::fmt;
use crate::model::{LessonsTab, Model};
use crate::view::{cursor, empty, hint, list};

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(5)])
        .split(area);
    match model.lessons.tab {
        LessonsTab::Lessons => lessons(model, frame, rows[0]),
        LessonsTab::Proposals => proposals(model, frame, rows[0]),
    }
    detail(model, frame, rows[1]);
}

fn tab_title(model: &Model, active: LessonsTab) -> String {
    let mark = |tab: LessonsTab, label: &str, count: usize| {
        if tab == active {
            format!("[{label} {count}]")
        } else {
            format!(" {label} {count} ")
        }
    };
    format!(
        " {}{} ",
        mark(LessonsTab::Lessons, "Lessons", model.lessons.lessons.len()),
        mark(
            LessonsTab::Proposals,
            "Proposals",
            model.lessons.proposals.len()
        )
    )
}

fn lessons(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let width = usize::from(area.width).saturating_sub(34);
    let lines: Vec<Line<'static>> = if model.lessons.lessons.is_empty() {
        empty(model, "  no lessons yet")
    } else {
        model
            .lessons
            .lessons
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let selected = index == model.lessons.lesson_selected;
                Line::from(Span::styled(
                    format!(
                        "{}{:<9} {:>4.2}  {:<width$}  {}",
                        cursor(selected),
                        fmt::truncate(&item.kind, 9),
                        item.importance,
                        fmt::truncate(&item.content, width),
                        fmt::truncate(&item.tags.join(","), 12),
                    ),
                    if selected {
                        model.theme.selected()
                    } else {
                        model.theme.status("")
                    },
                ))
            })
            .collect()
    };
    let search = model
        .lessons
        .search
        .as_ref()
        .map(|q| format!("search={q} "))
        .unwrap_or_default();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!("{}{search}", tab_title(model, LessonsTab::Lessons)))
        .title_bottom(hint(
            model,
            " Tab proposals · d forget · / search memory · r refresh ",
        ));
    list(
        frame,
        area,
        block,
        None,
        lines,
        model.lessons.lesson_selected,
    );
}

fn proposals(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let width = usize::from(area.width).saturating_sub(25);
    let lines: Vec<Line<'static>> = if model.lessons.proposals.is_empty() {
        empty(model, "  no proposals")
    } else {
        model
            .lessons
            .proposals
            .iter()
            .enumerate()
            .map(|(index, proposal)| {
                let selected = index == model.lessons.proposal_selected;
                Line::from(Span::styled(
                    format!(
                        "{}{:<9} {:<10} {:<width$}",
                        cursor(selected),
                        fmt::truncate(&proposal.kind, 9),
                        fmt::truncate(&proposal.status, 10),
                        fmt::truncate(&proposal.body, width),
                    ),
                    if selected {
                        model.theme.selected()
                    } else {
                        model.theme.status(&proposal.status)
                    },
                ))
            })
            .collect()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .title(tab_title(model, LessonsTab::Proposals))
        .title_bottom(hint(
            model,
            " Tab lessons · A accept · X reject · r refresh ",
        ));
    list(
        frame,
        area,
        block,
        None,
        lines,
        model.lessons.proposal_selected,
    );
}

fn detail(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let (title, body) = match model.lessons.tab {
        LessonsTab::Lessons => (
            " Lesson ",
            model
                .lessons
                .lessons
                .get(model.lessons.lesson_selected)
                .map(|item| format!("{}\n\nsource: {}", item.content, item.source))
                .unwrap_or_default(),
        ),
        LessonsTab::Proposals => (
            " Proposal ",
            model
                .lessons
                .proposals
                .get(model.lessons.proposal_selected)
                .map(|proposal| {
                    format!(
                        "{}\n\nevaluation: {}",
                        proposal.body,
                        fmt::short_id(proposal.evaluation_id.as_uuid())
                    )
                })
                .unwrap_or_default(),
        ),
    };
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(Block::new().borders(Borders::ALL).title(title)),
        area,
    );
}
