//! The routing leaderboard (`plan/07-api-and-tui.md` §Screens).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

use crate::fmt;
use crate::model::Model;
use crate::view::{cursor, empty, hint, list};

const KIND: usize = 12;
const ALIAS: usize = 22;
const NUM: usize = 9;

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let header = Line::from(Span::styled(
        format!(
            "  {:<KIND$}  {:<ALIAS$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}",
            "KIND", "ALIAS", "ATTEMPTS", "SUCCESS", "QUALITY", "COST", "LATENCY", "SCORE"
        ),
        model.theme.heading(),
    ));

    let rows = model.routes.sorted();
    let lines: Vec<Line<'static>> = if rows.is_empty() {
        empty(model, "  no route scores yet")
    } else {
        rows.iter()
            .enumerate()
            .map(|(index, row)| {
                let selected = index == model.routes.selected;
                Line::from(Span::styled(
                    format!(
                        "{}{:<KIND$}  {:<ALIAS$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}  {:>NUM$}",
                        cursor(selected),
                        fmt::truncate(&row.kind, KIND),
                        fmt::truncate(&row.alias, ALIAS),
                        row.attempts,
                        fmt::percent(row.successes, row.attempts),
                        row.mean_quality
                            .map_or_else(|| fmt::UNKNOWN.to_owned(), |q| format!("{q:.2}")),
                        fmt::money(row.mean_cost_usd),
                        row.mean_wall_ms.map_or_else(
                            || fmt::UNKNOWN.to_owned(),
                            |ms| fmt::duration(i64::try_from(ms).unwrap_or(i64::MAX))
                        ),
                        row.sampled_score
                            .map_or_else(|| fmt::UNKNOWN.to_owned(), |s| format!("{s:.3}")),
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

    let kind = model
        .routes
        .kind_filter
        .as_ref()
        .map_or_else(|| "all kinds".to_owned(), |kind| format!("kind={kind}"));
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(
            " Routes · {kind} · sorted by {} ",
            model.routes.sort.label()
        ))
        .title_bottom(hint(model, " k change kind · s sort · r refresh "));
    list(
        frame,
        area,
        block,
        Some(header),
        lines,
        model.routes.selected,
    );
}
