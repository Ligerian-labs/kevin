//! The `workers doctor` table (`plan/07-api-and-tui.md` §Screens).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

use crate::fmt;
use crate::model::Model;
use crate::view::{cursor, empty, hint, list};

const KIND: usize = 10;
const FLAG: usize = 8;
const VERSION: usize = 16;

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let rest = usize::from(area.width)
        .saturating_sub(KIND + FLAG * 2 + VERSION + 14)
        .max(10);
    let header = Line::from(Span::styled(
        format!(
            "  {:<KIND$}  {:<FLAG$}  {:<FLAG$}  {:<VERSION$}  {:<rest$}",
            "WORKER", "ENABLED", "AUTH", "VERSION", "BINARY / PROBLEMS"
        ),
        model.theme.heading(),
    ));

    let lines: Vec<Line<'static>> = if model.workers.items.is_empty() {
        empty(model, "  no workers reported")
    } else {
        model
            .workers
            .items
            .iter()
            .enumerate()
            .map(|(index, worker)| {
                let selected = index == model.workers.selected;
                let status = if !worker.problems.is_empty() {
                    "failed"
                } else if worker.enabled {
                    "succeeded"
                } else {
                    "skipped"
                };
                let last = if worker.problems.is_empty() {
                    worker.binary.as_ref().map_or_else(
                        || fmt::UNKNOWN.to_owned(),
                        |path| path.display().to_string(),
                    )
                } else {
                    worker.problems.join("; ")
                };
                Line::from(Span::styled(
                    format!(
                        "{}{:<KIND$}  {:<FLAG$}  {:<FLAG$}  {:<VERSION$}  {:<rest$}",
                        cursor(selected),
                        fmt::truncate(&worker.kind, KIND),
                        if worker.enabled { "yes" } else { "no" },
                        worker
                            .auth_ready
                            .map_or(fmt::UNKNOWN, |ok| if ok { "yes" } else { "no" }),
                        fmt::truncate(worker.version.as_deref().unwrap_or(fmt::UNKNOWN), VERSION),
                        fmt::truncate(&last, rest),
                    ),
                    if selected {
                        model.theme.selected()
                    } else {
                        model.theme.status(status)
                    },
                ))
            })
            .collect()
    };

    let unhealthy = model
        .workers
        .items
        .iter()
        .filter(|worker| worker.enabled && !worker.problems.is_empty())
        .count();
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(
            " Workers ({} enabled, {unhealthy} unhealthy) ",
            model.workers.items.iter().filter(|w| w.enabled).count()
        ))
        .title_bottom(hint(model, " r refresh "));
    list(
        frame,
        area,
        block,
        Some(header),
        lines,
        model.workers.selected,
    );
}
