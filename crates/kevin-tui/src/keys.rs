//! Terminal-independent key events.
//!
//! The reducer is pure and must be testable without a terminal, so it never
//! sees a `crossterm::event::KeyEvent`. [`KeyPress`] is the small subset of key
//! information the keybinding table in `plan/07-api-and-tui.md` §4 needs;
//! [`KeyPress::from_crossterm`] is the only place that knows about crossterm.

/// A key, without modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable character (already shifted by the terminal).
    Char(char),
    /// Return / Enter.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Shift-Tab.
    BackTab,
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Home.
    Home,
    /// End.
    End,
    /// A function key (`F1`…).
    F(u8),
    /// Anything the TUI does not bind.
    Other,
}

/// One key press: a [`Key`] plus the modifiers that matter to Kevin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPress {
    /// Which key.
    pub key: Key,
    /// Control was held.
    pub ctrl: bool,
    /// Alt was held.
    pub alt: bool,
}

impl KeyPress {
    /// A press with no modifier.
    #[must_use]
    pub const fn new(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
        }
    }

    /// A `Ctrl-<key>` press.
    #[must_use]
    pub const fn ctrl(key: Key) -> Self {
        Self {
            key,
            ctrl: true,
            alt: false,
        }
    }

    /// A bare character press (no Ctrl, no Alt).
    #[must_use]
    pub const fn char(c: char) -> Self {
        Self::new(Key::Char(c))
    }

    /// The character of a bare `Char` press, when this is one.
    #[must_use]
    pub const fn plain_char(&self) -> Option<char> {
        match self.key {
            Key::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }

    /// Whether this is the bare character `c`.
    #[must_use]
    pub fn is_char(&self, c: char) -> bool {
        self.plain_char() == Some(c)
    }

    /// Translates a crossterm key event; `None` for key *releases* (Windows and
    /// kitty-protocol terminals report those and they must not act twice).
    #[must_use]
    pub fn from_crossterm(event: &crossterm::event::KeyEvent) -> Option<Self> {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

        if event.kind == KeyEventKind::Release {
            return None;
        }
        let key = match event.code {
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::BackTab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::F(n) => Key::F(n),
            _ => Key::Other,
        };
        Some(Self {
            key,
            ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
            alt: event.modifiers.contains(KeyModifiers::ALT),
        })
    }
}

/// One row of the global help overlay (`plan/07-api-and-tui.md` §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// Keys, as printed.
    pub keys: &'static str,
    /// What they do.
    pub action: &'static str,
}

/// A titled group of [`Binding`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingGroup {
    /// Section title.
    pub title: &'static str,
    /// The rows.
    pub rows: &'static [Binding],
}

macro_rules! bindings {
    ($($keys:literal => $action:literal),* $(,)?) => {
        &[$(Binding { keys: $keys, action: $action }),*]
    };
}

/// The full keybinding table from `plan/07-api-and-tui.md` §4, rendered by the
/// help overlay and asserted by the snapshot tests.
pub const HELP: &[BindingGroup] = &[
    BindingGroup {
        title: "Global",
        rows: bindings! {
            "1..6"    => "switch screen",
            "?"       => "question inbox",
            ":"       => "command palette (:cancel, :answer)",
            "g / G"   => "top / bottom",
            "L"       => "client log pane (errors, reconnects)",
            "h / F1"  => "this help",
            "Ctrl-c / Q" => "quit (never cancels a run)",
        },
    },
    BindingGroup {
        title: "Runs",
        rows: bindings! {
            "Enter" => "open the run",
            "n"     => "new run (prompt)",
            "c"     => "cancel the run",
            "/"     => "filter by status",
            "r"     => "refresh",
        },
    },
    BindingGroup {
        title: "Run detail",
        rows: bindings! {
            "Tab"   => "cycle panes",
            "j / k" => "move",
            "Enter" => "focus the task",
            "f"     => "toggle follow",
            "a"     => "approve the plan",
            "x"     => "reject the plan (feedback)",
            "R"     => "retry the task",
            "C"     => "cancel the run / task",
            "q"     => "question inbox",
            "o"     => "open the artifact path",
            "y"     => "yank the id",
        },
    },
    BindingGroup {
        title: "Question inbox",
        rows: bindings! {
            "j / k" => "select",
            "Space" => "toggle (multi-select)",
            "Enter" => "choose / submit",
            "t"     => "free text",
            "Esc"   => "back",
        },
    },
    BindingGroup {
        title: "Plan approval",
        rows: bindings! {
            "a"     => "approve",
            "x"     => "reject with feedback",
            "Enter" => "expand the task",
            "Esc"   => "later",
        },
    },
    BindingGroup {
        title: "Routes / lessons / workers",
        rows: bindings! {
            "k"     => "change kind (routes)",
            "j / ↑↓" => "move (routes)",
            "s"     => "sort (routes)",
            "Tab"   => "lessons ⇄ proposals",
            "A / X" => "accept / reject a proposal",
            "d"     => "forget a lesson",
            "/"     => "search memory",
            "r"     => "refresh (workers)",
        },
    },
];

#[cfg(test)]
mod tests {
    use super::{HELP, Key, KeyPress};

    #[test]
    fn plain_char_ignores_modified_presses() {
        assert_eq!(KeyPress::char('a').plain_char(), Some('a'));
        assert_eq!(KeyPress::ctrl(Key::Char('a')).plain_char(), None);
    }

    #[test]
    fn help_covers_every_documented_group() {
        assert_eq!(HELP.len(), 6);
        assert!(HELP.iter().all(|group| !group.rows.is_empty()));
    }
}
