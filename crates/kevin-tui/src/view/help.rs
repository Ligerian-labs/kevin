//! The global help overlay: the keybinding table of `plan/07-api-and-tui.md`
//! §4, rendered from [`crate::keys::HELP`] so the code and the doc cannot drift.
//!
//! The table does not fit a 24-row terminal in one column, so a wide modal
//! splits it in two; a narrow one keeps one column and says how many rows it
//! had to cut.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::keys::{BindingGroup, HELP};
use crate::model::Model;
use crate::view::{hint, modal};

/// Below this inner width the overlay stays single-column.
const TWO_COLUMN_WIDTH: u16 = 96;
/// Groups shown in the left column when there are two.
const LEFT: [usize; 3] = [0, 2, 1];
/// Groups shown in the right column.
const RIGHT: [usize; 3] = [3, 4, 5];

pub(super) fn view(model: &Model, frame: &mut Frame<'_>, area: Rect) {
    let area = modal(area, 88, 92);
    frame.render_widget(Clear, area);

    let block = Block::new()
        .borders(Borders::ALL)
        .title(" Keybindings ")
        .title_bottom(hint(model, " Esc / h close "));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width >= TWO_COLUMN_WIDTH {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(inner);
        column(model, frame, columns[0], &LEFT);
        column(model, frame, columns[1], &RIGHT);
        return;
    }
    let all: Vec<usize> = (0..HELP.len()).collect();
    column(model, frame, inner, &all);
}

fn column(model: &Model, frame: &mut Frame<'_>, area: Rect, groups: &[usize]) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for index in groups {
        let Some(group) = HELP.get(*index) else {
            continue;
        };
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.extend(rows(model, group));
    }
    let height = usize::from(area.height).max(1);
    let cut = lines.len().saturating_sub(height);
    let mut visible: Vec<Line<'static>> = lines.into_iter().take(height).collect();
    if cut > 0 {
        visible.pop();
        visible.push(Line::from(Span::styled(
            format!("  … {} more rows (widen the terminal)", cut + 1),
            model.theme.dim(),
        )));
    }
    frame.render_widget(Paragraph::new(visible), area);
}

fn rows(model: &Model, group: &BindingGroup) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        group.title.to_owned(),
        model.theme.heading(),
    ))];
    lines.extend(group.rows.iter().map(|row| {
        Line::from(vec![
            Span::styled(format!("  {:<12}", row.keys), model.theme.heading()),
            Span::raw(row.action.to_owned()),
        ])
    }));
    lines
}
