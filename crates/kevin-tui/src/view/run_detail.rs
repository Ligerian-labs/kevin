//! Run detail: phase timeline, task board, live transcript and the cost footer
//! (`plan/07-api-and-tui.md` §Screens).

use kevin_api::dto::TaskDto;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, LineGauge, Paragraph, Wrap};

use crate::fmt;
use crate::model::{Model, Pane};
use crate::view::{cursor, empty, hint, list, pane, row_style, runs::status_name};

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let Some(run) = model.current_run() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no run open — pick one on the Runs screen (1)".to_owned(),
                model.theme.dim(),
            )))
            .block(Block::new().borders(Borders::ALL).title(" Run ")),
            area,
        );
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(4)])
        .split(area);

    // `plan/07` §Rendering rules: below 80×24 the panes collapse to one column.
    if model.is_narrow() {
        single_pane(model, frame, rows[0]);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(26),
                Constraint::Min(24),
                Constraint::Percentage(40),
            ])
            .split(rows[0]);
        timeline(model, frame, panes[0]);
        board(model, frame, panes[1]);
        transcript(model, frame, panes[2]);
    }
    footer(model, frame, rows[1], run);
}

fn single_pane(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    match model.detail.pane {
        Pane::Timeline => timeline(model, frame, area),
        Pane::Board => board(model, frame, area),
        Pane::Transcript => transcript(model, frame, area),
    }
}

fn timeline(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let focused = model.detail.pane == Pane::Timeline;
    let width = usize::from(area.width).saturating_sub(13);
    let lines: Vec<Line<'static>> = if model.detail.timeline.is_empty() {
        empty(model, "  waiting for events")
    } else {
        model
            .detail
            .timeline
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let selected = focused && index == model.detail.timeline_selected;
                Line::from(vec![
                    Span::raw(cursor(selected)),
                    Span::styled(format!("{} ", fmt::clock(entry.at)), model.theme.dim()),
                    Span::styled(
                        fmt::truncate(&entry.event_type, width),
                        model.theme.status(phase_of(&entry.event_type)),
                    ),
                ])
            })
            .collect()
    };
    let block = pane(
        model,
        format!("Timeline ({})", model.detail.timeline.len()),
        focused,
    )
    .title_bottom(hint(model, " C cancel run "));
    list(
        frame,
        area,
        block,
        None,
        lines,
        model.detail.timeline_selected,
    );
}

/// The status colour an event name maps to.
fn phase_of(event_type: &str) -> &str {
    match event_type {
        t if t.ends_with("completed") || t.ends_with("succeeded") => "succeeded",
        t if t.ends_with("failed") => "failed",
        t if t.ends_with("cancelled") || t.ends_with("skipped") => "cancelled",
        t if t.contains("question") || t.contains("approval") => "awaiting_input",
        _ => "running",
    }
}

fn board(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let focused = model.detail.pane == Pane::Board;
    let width = usize::from(area.width).saturating_sub(2);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut flat = 0usize;
    let mut selected_row = 0usize;

    for (status, tasks) in model.detail.groups() {
        lines.push(Line::from(Span::styled(
            format!("{status} ({})", tasks.len()),
            model.theme.status(&status),
        )));
        for task in tasks {
            let selected = flat == model.detail.board_selected;
            if selected {
                selected_row = lines.len();
            }
            lines.push(Line::from(Span::styled(
                fmt::truncate(&task_row(model, task), width),
                row_style(model, focused && selected, &task.status),
            )));
            flat += 1;
        }
    }
    if lines.is_empty() {
        lines = empty(model, "  no tasks yet");
    }

    let block = pane(
        model,
        format!("Tasks ({})", model.detail.tasks.len()),
        focused,
    )
    .title_bottom(hint(
        model,
        " Enter focus · R retry · C cancel · f follow · Tab pane ",
    ));
    list(frame, area, block, None, lines, selected_row);
}

fn task_row(model: &Model, task: &TaskDto) -> String {
    let route = task.route.as_ref().map_or_else(
        || fmt::UNKNOWN.to_owned(),
        |route| format!("{}/{}", route.worker, route.model),
    );
    let attempt = task.attempts.len();
    let elapsed = task.attempts.last().map_or_else(
        || fmt::UNKNOWN.to_owned(),
        |attempt| {
            let end = attempt.ended_at.unwrap_or(model.now);
            fmt::duration(
                end.signed_duration_since(attempt.started_at)
                    .num_milliseconds(),
            )
        },
    );
    let focused = model.detail.focused_task == Some(task.id);
    format!(
        "{}{} {} · {route} #{attempt} {elapsed}",
        cursor(focused),
        task.kind,
        task.title,
    )
}

fn transcript(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let focused = model.detail.pane == Pane::Transcript;
    let title = model.detail.focused().map_or_else(
        || "Transcript".to_owned(),
        |task| format!("Transcript · {}", fmt::truncate(&task.title, 24)),
    );
    let follow = if model.detail.follow {
        "follow ON"
    } else {
        "follow OFF"
    };
    let height = usize::from(area.height.saturating_sub(2)).max(1);
    let width = usize::from(area.width).saturating_sub(13);

    let lines: Vec<Line<'static>> = if model.detail.focused_task.is_none() {
        empty(model, "  Enter on a task to follow its transcript")
    } else if model.detail.log.is_empty() {
        empty(model, "  no output yet")
    } else {
        let end = model
            .detail
            .log
            .len()
            .saturating_sub(model.detail.log_scroll);
        let start = end.saturating_sub(height);
        (start..end)
            .filter_map(|index| model.detail.log.get(index))
            .map(|line| {
                Line::from(vec![
                    Span::styled(format!("{} ", fmt::clock(line.at)), model.theme.dim()),
                    Span::styled(format!("{:<13}", line.kind), model.theme.dim()),
                    Span::raw(fmt::truncate(&fmt::log_payload(&line.payload), width)),
                ])
            })
            .collect()
    };

    let dropped = model.detail.log.dropped();
    let counter = format!(
        " {follow} · {}/{} lines{} ",
        model.detail.log.len(),
        model.detail.log.capacity(),
        if dropped > 0 {
            format!(" · {dropped} dropped")
        } else {
            String::new()
        }
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane(model, title, focused).title_bottom(hint(model, &counter))),
        area,
    );
}

fn footer(model: &Model, frame: &mut Frame<'_>, area: Rect, run: &kevin_api::dto::RunDto) {
    let block = Block::new()
        .borders(Borders::ALL)
        .title(format!(
            " {} · {} ",
            fmt::short_id(run.id.as_uuid()),
            status_name(run.status)
        ))
        .title_bottom(hint(model, " a approve · x reject · q inbox · y yank "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let usage = &run.usage;
    // `GET /api/v1/cost?run_id=` breaks the spend down; name the top spender so
    // the footer says *where* the money went, not only how much.
    let top = model
        .detail
        .cost
        .as_ref()
        .and_then(|report| report.rows.first())
        .map(|row| format!(" · top {} {}", row.key, fmt::money(row.usd)))
        .unwrap_or_default();
    let text = format!(
        "{} · {} in / {} out tokens · {} tasks{top} · {}",
        fmt::money_of(usage.cost_usd, run.budget.max_usd),
        fmt::tokens(usage.input_tokens),
        fmt::tokens(usage.output_tokens),
        run.tasks.len(),
        fmt::truncate(&run.goal.text, 40),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(fmt::truncate(
            &text,
            usize::from(rows[0].width),
        ))))
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let gauges = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);
    let budget = fmt::ratio(usage.cost_usd, run.budget.max_usd).unwrap_or(0.0);
    let wall = fmt::ratio_u64(usage.wall_ms, run.budget.max_wall_ms).unwrap_or(0.0);
    frame.render_widget(
        LineGauge::default()
            .ratio(budget)
            .label(format!("budget {:>3.0}%", budget * 100.0)),
        gauges[0],
    );
    frame.render_widget(
        LineGauge::default()
            .ratio(wall)
            .label(format!("wall {:>3.0}%", wall * 100.0)),
        gauges[1],
    );
}
