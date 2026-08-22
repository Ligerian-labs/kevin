//! Shared human/JSON output helpers for the run-facing subcommands
//! (`plan/07-api-and-tui.md` §3: tables when stdout is a TTY, one JSON object
//! — or one JSON object per line for streams — with `--json`).

use std::io::{IsTerminal as _, Write as _};

use chrono::{DateTime, Utc};
use comfy_table::{ContentArrangement, Table, presets};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Prints one JSON value on its own line and flushes, so a piped consumer sees
/// stream items as they happen.
pub fn json_line(value: &serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Prints one human line and flushes.
pub fn line(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

/// A borderless table sized to the terminal.
#[must_use]
pub fn table(headers: &[&str], rows: Vec<Vec<String>>) -> String {
    let mut table = Table::new();
    table
        .load_style(presets::NOTHING)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(headers.iter().map(|h| h.to_uppercase()));
    for row in rows {
        table.add_row(row);
    }
    table.to_string()
}

/// Whether stdout is attached to a terminal.
#[must_use]
pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Whether stdin is attached to a terminal.
#[must_use]
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// The first eight characters of a uuid, for tables.
#[must_use]
pub fn short(id: Uuid) -> String {
    id.simple().to_string()[..8].to_owned()
}

/// Money as a decimal string, `-` when unknown (never a float).
#[must_use]
pub fn money(usd: Option<Decimal>) -> String {
    usd.map_or_else(|| "-".to_owned(), |d| format!("{d:.4}"))
}

/// `HH:MM:SS` of a timestamp, local to the event stream's own clock (UTC).
#[must_use]
pub fn clock(at: DateTime<Utc>) -> String {
    at.format("%H:%M:%S").to_string()
}

/// A compact age (`3s`, `4m`, `2h`, `5d`).
#[must_use]
pub fn age(since: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - since).num_seconds().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// A wall-clock duration in milliseconds, rendered compactly.
#[must_use]
pub fn millis(ms: i64) -> String {
    let seconds = ms / 1000;
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{}s", s / 60, s % 60),
        s => format!("{}h{}m", s / 3600, (s % 3600) / 60),
    }
}
