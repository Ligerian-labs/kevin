//! The plan-approval modal (`plan/07-api-and-tui.md` §Screens).
//!
//! The task DAG is drawn as an indented tree in topological order; each row
//! shows the kind, the title, the suggested tier, the parallel-safe flag and
//! how many acceptance criteria it carries. Dependencies are named after the
//! row (`← dep titles`). The rationale panel sits underneath.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::fmt;
use crate::model::Model;
use crate::plan::PlanView;
use crate::view::{cursor, empty, hint, list, modal};

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let area = modal(area, 84, 80);
    frame.render_widget(Clear, area);

    let Some(run) = model.current_run() else {
        return;
    };
    let title = format!(" Plan approval · run {} ", fmt::short_id(run.id.as_uuid()));
    let Some(plan) = run.plan.as_ref() else {
        frame.render_widget(
            Paragraph::new(empty(model, "  this run has no plan yet"))
                .block(Block::new().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    };
    let view = PlanView::parse(plan);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(5)])
        .split(area);

    let width = usize::from(rows[0].width).saturating_sub(4);
    let lines: Vec<Line<'static>> = if view.is_empty() {
        vec![Line::from(Span::raw(fmt::truncate(
            &plan.0.to_string(),
            width,
        )))]
    } else {
        view.tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                let selected = index == model.detail.board_selected;
                let indent = "  ".repeat(task.depth);
                let deps = view.dependency_titles(task);
                let deps = if deps.is_empty() {
                    String::new()
                } else {
                    format!("  ← {}", deps.join(", "))
                };
                let tier = task.suggested_tier.as_deref().unwrap_or(fmt::UNKNOWN);
                let parallel = if task.parallel_safe { "∥" } else { "→" };
                // `allow_push` widens the blast radius past the workspace, so
                // it is called out rather than hidden (`plan/09` §Workspace
                // isolation: "flagged in the plan approval view").
                let push = if task.allow_push { " · PUSH" } else { "" };
                Line::from(Span::styled(
                    fmt::truncate(
                        &format!(
                            "{}{indent}{} {} · {tier} {parallel} · {} criteria{push}{deps}",
                            cursor(selected),
                            task.kind,
                            task.title,
                            task.acceptance_criteria.len(),
                        ),
                        width,
                    ),
                    if selected {
                        model.theme.selected()
                    } else {
                        model.theme.status("awaiting_plan_approval")
                    },
                ))
            })
            .collect()
    };

    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!("{title}· {} tasks ", view.tasks.len()))
        .title_bottom(hint(
            model,
            " a approve · x reject with feedback · Enter expand · Esc later ",
        ));
    list(
        frame,
        rows[0],
        block,
        None,
        lines,
        model.detail.board_selected,
    );

    let selected = view.tasks.get(model.detail.board_selected);
    let rationale = selected.map_or_else(
        || view.rationale.clone(),
        |task| {
            if task.acceptance_criteria.is_empty() {
                view.rationale.clone()
            } else {
                task.acceptance_criteria
                    .iter()
                    .map(|criterion| format!("· {criterion}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        },
    );
    frame.render_widget(
        Paragraph::new(rationale).wrap(Wrap { trim: false }).block(
            Block::new().borders(Borders::ALL).title(
                if selected.is_some_and(|task| !task.acceptance_criteria.is_empty()) {
                    " Acceptance criteria "
                } else {
                    " Rationale "
                },
            ),
        ),
        rows[1],
    );
}
