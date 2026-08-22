//! Colours (`plan/07-api-and-tui.md` §Rendering rules).
//!
//! running = yellow, succeeded/completed = green, failed = red, `awaiting_*` =
//! magenta, cancelled/skipped = dim. `NO_COLOR` in the environment switches
//! every style to the monochrome fallback.

use ratatui::style::{Color, Modifier, Style};

/// Whether the session paints colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    /// `false` when `NO_COLOR` is set.
    pub color: bool,
}

impl Theme {
    /// The colourful theme.
    pub const COLOR: Self = Self { color: true };
    /// The `NO_COLOR` theme.
    pub const MONO: Self = Self { color: false };

    /// Reads `NO_COLOR` from the process environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            color: std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn fg(self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    /// Emphasis used for headers and pane titles.
    #[must_use]
    pub fn heading(self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// De-emphasis (hints, cancelled/skipped rows).
    #[must_use]
    pub fn dim(self) -> Style {
        Style::default().add_modifier(Modifier::DIM)
    }

    /// The highlighted row of a list or table.
    #[must_use]
    pub fn selected(self) -> Style {
        let style = Style::default().add_modifier(Modifier::REVERSED);
        if self.color {
            style.fg(Color::Cyan)
        } else {
            style
        }
    }

    /// An error line.
    #[must_use]
    pub fn error(self) -> Style {
        self.fg(Color::Red).add_modifier(Modifier::BOLD)
    }

    /// The style for a run or task status, per the rendering rules.
    #[must_use]
    pub fn status(self, status: &str) -> Style {
        match status {
            "running" | "executing" | "understanding" | "planning" | "integrating"
            | "evaluating" | "routed" => self.fg(Color::Yellow),
            "succeeded" | "completed" | "accepted" => self.fg(Color::Green),
            "failed" | "rejected" => self.fg(Color::Red),
            s if s.starts_with("awaiting") => self.fg(Color::Magenta),
            "cancelled" | "skipped" => self.dim(),
            _ => Style::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};

    use super::Theme;

    #[test]
    fn no_color_drops_every_foreground() {
        assert_eq!(Theme::MONO.status("running"), Style::default());
        assert_eq!(
            Theme::COLOR.status("running"),
            Style::default().fg(Color::Yellow)
        );
    }

    #[test]
    fn awaiting_states_are_magenta() {
        assert_eq!(
            Theme::COLOR.status("awaiting_plan_approval"),
            Style::default().fg(Color::Magenta)
        );
    }
}
