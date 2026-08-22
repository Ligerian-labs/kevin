//! Rendering. `view(&Model, &mut Frame)` is a pure function of the model, so
//! every screen is snapshot-testable with `ratatui::backend::TestBackend`
//! (`plan/07-api-and-tui.md` §Tests).

mod help;
mod inbox;
mod lessons;
mod plan;
mod routes;
mod run_detail;
mod runs;
mod workers;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};

use crate::fmt;
use crate::model::{Level, Model, Overlay, Screen};

/// Height of the client-log pane when it is open (`L`).
const LOG_PANE_HEIGHT: u16 = 7;

/// Draws the whole frame.
pub fn view(model: &Model, frame: &mut Frame<'_>) {
    let area = frame.area();
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(3)];
    if model.show_client_log {
        constraints.push(Constraint::Length(LOG_PANE_HEIGHT));
    }
    constraints.push(Constraint::Length(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    tabs(model, frame, rows[0]);
    body(model, frame, rows[1]);
    if model.show_client_log {
        client_log(model, frame, rows[2]);
    }
    footer(model, frame, rows[rows.len() - 1]);

    if let Some(overlay) = model.overlay.as_ref() {
        match overlay {
            Overlay::Help => help::view(model, frame, area),
            Overlay::PlanApproval => plan::view(model, frame, area),
            Overlay::NewRun(input) => prompt(
                model,
                frame,
                area,
                "New run",
                input,
                "Enter run · Esc cancel",
            ),
            Overlay::Filter(input) => prompt(
                model,
                frame,
                area,
                "Filter runs by status",
                input,
                "Enter apply · empty clears · Esc cancel",
            ),
            Overlay::RejectFeedback(input) => prompt(
                model,
                frame,
                area,
                "Reject plan — what should change?",
                input,
                "Enter send · Esc cancel",
            ),
            Overlay::FreeText(input) => prompt(
                model,
                frame,
                area,
                "Free-text answer",
                input,
                "Enter keep · Esc cancel · then Enter in the inbox submits",
            ),
            Overlay::MemorySearch(input) => prompt(
                model,
                frame,
                area,
                "Search memory",
                input,
                "Enter search · empty restores the lessons page · Esc cancel",
            ),
        }
    }
}

fn tabs(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let titles: Vec<Line<'_>> = Screen::all()
        .iter()
        .map(|screen| Line::from(screen.title()))
        .collect();
    let index = Screen::all()
        .iter()
        .position(|screen| *screen == model.screen)
        .unwrap_or(0);
    frame.render_widget(
        Tabs::new(titles)
            .select(index)
            .divider("│")
            .highlight_style(model.theme.selected()),
        area,
    );
}

fn body(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    match model.screen {
        Screen::Runs => runs::view(model, frame, area),
        Screen::RunDetail => run_detail::view(model, frame, area),
        Screen::Questions => inbox::view(model, frame, area),
        Screen::Routes => routes::view(model, frame, area),
        Screen::Lessons => lessons::view(model, frame, area),
        Screen::Workers => workers::view(model, frame, area),
    }
}

fn client_log(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let height = usize::from(area.height.saturating_sub(2));
    let lines: Vec<Line<'_>> = model
        .client_log
        .tail(height)
        .map(|line| {
            let style = if line.level == Level::Error {
                model.theme.error()
            } else {
                model.theme.dim()
            };
            Line::from(vec![
                Span::styled(format!("{} ", fmt::clock(line.at)), model.theme.dim()),
                Span::styled(line.text.clone(), style),
            ])
        })
        .collect();
    let title = format!(
        " client log ({} lines, {} dropped, {} resyncs) ",
        model.client_log.len(),
        model.client_log.dropped(),
        model.resync_count
    );
    frame.render_widget(
        Paragraph::new(lines).block(Block::new().borders(Borders::ALL).title(title)),
        area,
    );
}

fn footer(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    if let Some(status) = model.status.as_ref() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fmt::truncate(status, usize::from(area.width)),
                model.theme.error(),
            ))),
            area,
        );
        return;
    }
    let open_questions = model.inbox.items.len();
    let draining = if model.drain.as_ref().is_some_and(|status| status.draining) {
        " · DRAINING"
    } else {
        ""
    };
    let server = if model.server.is_empty() {
        "(no server)"
    } else {
        model.server.as_str()
    };
    let text = format!(
        "{server}{draining} · {open_questions} open question(s) · h help · 1..6 screens · Q quit"
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fmt::truncate(&text, usize::from(area.width)),
            model.theme.dim(),
        ))),
        area,
    );
}

fn prompt(
    model: &Model,
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    input: &crate::model::TextInput,
    keys: &str,
) {
    let area = centered(area, 70, 5);
    frame.render_widget(Clear, area);
    let text = vec![
        Line::from(vec![
            Span::styled(format!("{}: ", input.label), model.theme.heading()),
            Span::raw(input.value.clone()),
            Span::raw("▏"),
        ]),
        Line::from(Span::styled(keys.to_owned(), model.theme.dim())),
    ];
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::new()
                .borders(Borders::ALL)
                .title(format!(" {title} ")),
        ),
        area,
    );
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A `width`×`height` rectangle in the middle of `area`, clipped to it.
pub(crate) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(horizontal);
    rect
}

/// A modal covering `percent_x`/`percent_y` of `area`.
pub(crate) fn modal(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    centered(
        area,
        area.width * percent_x / 100,
        area.height * percent_y / 100,
    )
}

/// A bordered block whose title is bold when the pane has the keyboard.
pub(crate) fn pane<'a>(model: &Model, title: impl Into<String>, focused: bool) -> Block<'a> {
    let marker = if focused { "▌" } else { " " };
    let style = if focused {
        model.theme.heading()
    } else {
        Style::default()
    };
    Block::new()
        .borders(Borders::ALL)
        .title(Span::styled(format!("{marker}{} ", title.into()), style))
}

/// Renders `lines` inside `area`, pinning `header` and scrolling just enough to
/// keep row `selected` visible.
pub(crate) fn list(
    frame: &mut Frame<'_>,
    area: Rect,
    block: Block<'_>,
    header: Option<Line<'static>>,
    lines: Vec<Line<'static>>,
    selected: usize,
) {
    let pinned = usize::from(header.is_some());
    let inner_height = usize::from(area.height.saturating_sub(2))
        .saturating_sub(pinned)
        .max(1);
    let offset = selected.saturating_sub(inner_height.saturating_sub(1));
    let mut visible: Vec<Line<'static>> = header.into_iter().collect();
    visible.extend(lines.into_iter().skip(offset).take(inner_height));
    frame.render_widget(Paragraph::new(visible).block(block), area);
}

/// The keybinding hint every screen and modal prints along its bottom border.
pub(crate) fn hint<'a>(model: &Model, text: &'a str) -> Line<'a> {
    Line::from(Span::styled(text, model.theme.dim()))
}

/// The `▸ ` / `  ` prefix of a list row.
pub(crate) fn cursor(selected: bool) -> &'static str {
    if selected { "▸ " } else { "  " }
}

/// A row style: the selection wins over the status colour.
pub(crate) fn row_style(model: &Model, selected: bool, status: &str) -> Style {
    if selected {
        model.theme.selected()
    } else {
        model.theme.status(status)
    }
}

/// An empty-state line.
pub(crate) fn empty(model: &Model, text: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(text.to_owned(), model.theme.dim()))]
}
