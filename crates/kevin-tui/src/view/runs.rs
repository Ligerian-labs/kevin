//! The runs list — the home screen (`plan/07-api-and-tui.md` §Screens).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

use crate::fmt;
use crate::model::Model;
use crate::view::{cursor, empty, hint, list, row_style};

/// Fixed-width columns; the goal takes whatever is left.
const ID: usize = 8;
const STATUS: usize = 20;
const TASKS: usize = 7;
const COST: usize = 9;
const AGE: usize = 6;

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let goal_width = usize::from(area.width)
        .saturating_sub(ID + STATUS + TASKS + COST + AGE + 14)
        .max(8);

    let header = Line::from(Span::styled(
        format!(
            "  {:<ID$}  {:<STATUS$}  {:<goal_width$}  {:>TASKS$}  {:>COST$}  {:>AGE$}",
            "RUN", "STATUS", "GOAL", "TASKS", "COST", "AGE"
        ),
        model.theme.heading(),
    ));

    let rows: Vec<Line<'static>> = if model.runs.items.is_empty() {
        empty(model, "  no runs yet — press `n` to start one")
    } else {
        model
            .runs
            .items
            .iter()
            .enumerate()
            .map(|(index, run)| {
                let selected = index == model.runs.selected;
                let status = status_name(run.status);
                let counts = run.task_counts;
                let done = counts.succeeded + counts.failed + counts.cancelled + counts.skipped;
                Line::from(Span::styled(
                    format!(
                        "{}{:<ID$}  {:<STATUS$}  {:<goal_width$}  {:>TASKS$}  {:>COST$}  {:>AGE$}",
                        cursor(selected),
                        fmt::short_id(run.id.as_uuid()),
                        fmt::truncate(&status, STATUS),
                        fmt::truncate(&run.goal_excerpt, goal_width),
                        format!("{done}/{}", counts.total),
                        fmt::money(run.usage.cost_usd),
                        fmt::age(model.now, run.created_at),
                    ),
                    row_style(model, selected, &status),
                ))
            })
            .collect()
    };

    let filter = model
        .runs
        .status_filter
        .as_ref()
        .map(|status| format!("[status={status}] "))
        .unwrap_or_default();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(" Runs ({}) {filter}", model.runs.items.len()))
        .title_bottom(hint(
            model,
            " Enter open · n new · c cancel · / filter · r refresh · y yank ",
        ));
    list(frame, area, block, Some(header), rows, model.runs.selected);
}

/// The serde name of a run status (`executing`, `awaiting_plan_approval`, …).
pub(crate) fn status_name(status: kevin_api::dto::RunStatusDto) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}
