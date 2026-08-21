//! Rendering of the route leaderboard and of a selection explanation
//! (`plan/06-memory-and-learning.md` §2.5 — the output of `kevin routes` and
//! `kevin routes explain`).

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use kevin_domain::Decimal;

use crate::router::{CandidateScore, RouteSelection};
use crate::score::LeaderboardRow;

/// Header of the `kevin routes` table.
pub const LEADERBOARD_HEADER: [&str; 9] = [
    "KIND",
    "ALIAS",
    "N",
    "WIN%",
    "P(SUCC)",
    "QUALITY",
    "$/TASK",
    "AVG WALL",
    "LAST USED",
];

/// Header of the `kevin routes explain` table.
pub const EXPLAIN_HEADER: [&str; 8] = [
    "ALIAS", "TIER", "N", "SAMPLED", "QUALITY", "NCOST", "NLAT", "SCORE",
];

/// Renders the leaderboard as a fixed-width table (empty rows → a hint).
#[must_use]
pub fn render_leaderboard(rows: &[LeaderboardRow], now: DateTime<Utc>) -> String {
    if rows.is_empty() {
        return "no route scores yet — run a task and Kevin starts learning\n".to_owned();
    }
    let mut table: Vec<Vec<String>> =
        vec![LEADERBOARD_HEADER.iter().map(|h| (*h).to_owned()).collect()];
    for row in rows {
        let stats = &row.stats;
        table.push(vec![
            row.task_kind.to_string(),
            row.alias.to_string(),
            stats.attempts.to_string(),
            percent(stats.win_rate()),
            format!("{:.2}", stats.p_success()),
            stats
                .quality_ema
                .map_or_else(|| "n/a".to_owned(), |q| format!("{q:.2}")),
            stats
                .mean_cost_usd()
                .map_or_else(|| "n/a".to_owned(), format_usd),
            stats
                .mean_wall_ms()
                .map_or_else(|| "n/a".to_owned(), format_duration_ms),
            stats
                .last_used
                .map_or_else(|| "never".to_owned(), |t| format_ago(now, t)),
        ]);
    }
    render(&table)
}

/// Renders a dry-run selection: the candidate table plus the decision line.
#[must_use]
pub fn render_explain(selection: &RouteSelection) -> String {
    let mut table: Vec<Vec<String>> =
        vec![EXPLAIN_HEADER.iter().map(|h| (*h).to_owned()).collect()];
    for candidate in &selection.candidates {
        table.push(vec![
            format!(
                "{}{}",
                if candidate.selected { "→ " } else { "  " },
                candidate.alias
            ),
            candidate.tier.to_string(),
            candidate.samples.to_string(),
            format!("{:.3}", candidate.sampled_success),
            format!("{:.2}", candidate.quality),
            format!("{:.2}", candidate.norm_cost),
            format!("{:.2}", candidate.norm_latency),
            score_or_reason(candidate),
        ]);
    }
    let mut out = render(&table);
    let explored = if selection.explored {
        " (exploration floor)"
    } else {
        ""
    };
    let _ = write!(
        out,
        "\npolicy: {}{explored}\nroute:  {} (catalog {})\n",
        selection.policy,
        selection.route,
        short_version(&selection.catalog_version),
    );
    out
}

fn score_or_reason(candidate: &CandidateScore) -> String {
    candidate.excluded_reason.as_ref().map_or_else(
        || format!("{:.3}", candidate.score),
        |reason| format!("excluded: {reason}"),
    )
}

/// First 12 characters of a catalog version, enough to eyeball.
#[must_use]
pub fn short_version(version: &str) -> &str {
    let end = version.len().min(12);
    &version[..end]
}

fn render(table: &[Vec<String>]) -> String {
    let columns = table.first().map_or(0, Vec::len);
    let mut widths = vec![0usize; columns];
    for row in table {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in table {
        let last = row.len().saturating_sub(1);
        for (i, cell) in row.iter().enumerate() {
            if i == last {
                out.push_str(cell);
            } else {
                let pad = widths[i].saturating_sub(cell.chars().count());
                out.push_str(cell);
                out.push_str(&" ".repeat(pad + 2));
            }
        }
        out.push('\n');
    }
    out
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn percent(fraction: f32) -> String {
    format!("{}%", (fraction * 100.0).round() as u32)
}

fn format_usd(value: Decimal) -> String {
    format!("{:.2}", value.round_dp(2))
}

fn format_duration_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn format_ago(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let seconds = (now - then).num_seconds().max(0);
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use kevin_domain::route_score::BetaPrior;
    use kevin_domain::{ModelAlias, RouteStats, TaskKind};

    use super::*;

    fn row(alias: &str, attempts: u32, successes: u32) -> LeaderboardRow {
        let mut stats = RouteStats::from_prior(BetaPrior::UNIFORM);
        stats.attempts = attempts;
        stats.successes = successes;
        stats.alpha = 1.0 + f32::from(u16::try_from(successes).unwrap_or(0));
        stats.quality_ema = Some(0.78);
        stats.sum_wall_ms = u64::from(attempts) * 372_000;
        stats.last_used = Some(Utc::now() - TimeDelta::hours(2));
        LeaderboardRow {
            task_kind: TaskKind::Implement,
            alias: ModelAlias::new(alias).unwrap(),
            stats,
        }
    }

    #[test]
    fn leaderboard_renders_a_header_and_one_line_per_row() {
        let rows = vec![row("sonnet5-claude", 14, 12), row("gpt56-codex", 9, 7)];
        let out = render_leaderboard(&rows, Utc::now());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("KIND"));
        assert!(lines[0].contains("WIN%"));
        assert!(lines[0].contains("LAST USED"));
        assert!(lines[1].contains("implement"));
        assert!(lines[1].contains("sonnet5-claude"));
        assert!(lines[1].contains("86%"), "win rate: {}", lines[1]);
        assert!(lines[1].contains("n/a"), "unknown cost: {}", lines[1]);
        assert!(lines[1].contains("2h ago"));
    }

    #[test]
    fn empty_leaderboard_explains_itself() {
        assert!(render_leaderboard(&[], Utc::now()).contains("no route scores yet"));
    }

    #[test]
    fn durations_and_percentages_are_compact() {
        assert_eq!(format_duration_ms(45_000), "45s");
        assert_eq!(format_duration_ms(372_000), "6m12s");
        assert_eq!(format_duration_ms(3_960_000), "1h06m");
        assert_eq!(percent(0.857), "86%");
        assert_eq!(short_version("0123456789abcdef"), "0123456789ab");
    }
}
