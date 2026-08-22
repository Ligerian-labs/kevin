//! Small formatters shared by the screens.
//!
//! Money is a decimal string end to end (`plan/07-api-and-tui.md`
//! §Conventions); nothing here ever turns a cost into a float.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Placeholder for "the server does not know".
pub const UNKNOWN: &str = "—";

/// The first block of a uuid, which is what the screens show.
#[must_use]
pub fn short_id(id: Uuid) -> String {
    let text = id.hyphenated().to_string();
    text.split('-').next().unwrap_or(&text).to_owned()
}

/// `$1.2345`, or [`UNKNOWN`] when no price is known.
#[must_use]
pub fn money(value: Option<Decimal>) -> String {
    value.map_or_else(|| UNKNOWN.to_owned(), |value| format!("${value}"))
}

/// `$0.42 / $5.00`, with the cap omitted when there is none.
#[must_use]
pub fn money_of(spent: Option<Decimal>, cap: Option<Decimal>) -> String {
    match cap {
        Some(cap) => format!("{} / ${cap}", money(spent)),
        None => money(spent),
    }
}

/// `1234`, `12.3k`, `1.2M`.
#[must_use]
pub fn tokens(count: u64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "display only: a token count above 2^53 is not reachable"
    )]
    match count {
        0..=9_999 => count.to_string(),
        10_000..=999_999 => format!("{:.1}k", count as f64 / 1_000.0),
        _ => format!("{:.1}M", count as f64 / 1_000_000.0),
    }
}

/// `12s`, `3m`, `2h`, `4d` — the age column of the runs table.
#[must_use]
pub fn age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    duration(now.signed_duration_since(then).num_milliseconds().max(0))
}

/// A millisecond count as a compact duration.
#[must_use]
pub fn duration(ms: i64) -> String {
    let seconds = ms / 1_000;
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m{:02}s", s / 60, s % 60),
        s if s < 86_400 => format!("{}h{:02}m", s / 3_600, (s % 3_600) / 60),
        s => format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

/// `12:00:01` — timestamps in the timeline and the transcript.
#[must_use]
pub fn clock(at: DateTime<Utc>) -> String {
    at.format("%H:%M:%S").to_string()
}

/// Truncates to `width` characters, ending with `…` when it had to cut.
#[must_use]
pub fn truncate(text: &str, width: usize) -> String {
    let text = text.replace(['\n', '\r', '\t'], " ");
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text;
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `0.0..=1.0` for a gauge; `None` when the cap is unknown or zero.
#[must_use]
pub fn ratio(used: Option<Decimal>, cap: Option<Decimal>) -> Option<f64> {
    use rust_decimal::prelude::ToPrimitive as _;
    let cap = cap?;
    if cap.is_zero() {
        return None;
    }
    let used = used.unwrap_or_default();
    let ratio = (used / cap).to_f64()?;
    Some(ratio.clamp(0.0, 1.0))
}

/// `0.0..=1.0` for a gauge over integer counters.
#[must_use]
pub fn ratio_u64(used: u64, cap: Option<u64>) -> Option<f64> {
    let cap = cap?;
    if cap == 0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "display only: gauge ratios do not need full u64 precision"
    )]
    Some((used as f64 / cap as f64).clamp(0.0, 1.0))
}

/// A percentage with no decimals; `—` when there is nothing to divide.
#[must_use]
pub fn percent(numerator: u32, denominator: u32) -> String {
    if denominator == 0 {
        return UNKNOWN.to_owned();
    }
    format!("{}%", u64::from(numerator) * 100 / u64::from(denominator))
}

/// One transcript line, collapsed to a single row.
#[must_use]
pub fn log_payload(payload: &serde_json::Value) -> String {
    match payload {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Object(map) => map
            .get("text")
            .or_else(|| map.get("message"))
            .or_else(|| map.get("name"))
            .and_then(|value| value.as_str().map(ToOwned::to_owned))
            .unwrap_or_else(|| payload.to_string()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use rust_decimal::Decimal;

    use super::{age, money_of, percent, ratio, short_id, tokens, truncate};

    #[test]
    fn formats_money_with_and_without_a_cap() {
        assert_eq!(
            money_of(Some(Decimal::new(42, 2)), Some(Decimal::new(500, 2))),
            "$0.42 / $5.00"
        );
        assert_eq!(money_of(None, None), "—");
    }

    #[test]
    fn formats_ages_and_tokens() {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
        assert_eq!(age(now, now - chrono::Duration::seconds(30)), "30s");
        assert_eq!(age(now, now - chrono::Duration::minutes(5)), "5m00s");
        assert_eq!(age(now, now - chrono::Duration::hours(30)), "1d06h");
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(12_345), "12.3k");
        assert_eq!(tokens(2_500_000), "2.5M");
    }

    #[test]
    fn truncates_on_character_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("héllo", 3), "hé…");
        assert_eq!(truncate("a\nb", 5), "a b");
    }

    #[test]
    fn ratios_are_clamped_and_optional() {
        assert_eq!(ratio(Some(Decimal::ONE), Some(Decimal::TWO)), Some(0.5));
        assert_eq!(ratio(Some(Decimal::TEN), Some(Decimal::ONE)), Some(1.0));
        assert_eq!(ratio(Some(Decimal::ONE), None), None);
        assert_eq!(ratio(Some(Decimal::ONE), Some(Decimal::ZERO)), None);
        assert_eq!(percent(1, 0), "—");
        assert_eq!(percent(1, 4), "25%");
    }

    #[test]
    fn short_ids_are_the_first_uuid_block() {
        let id = uuid::uuid!("0191f3a1-0000-7000-8000-000000000001");
        assert_eq!(short_id(id), "0191f3a1");
    }
}
