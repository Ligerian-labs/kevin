//! Usage normalisation and the cost fallback hook (`plan/04-workers.md`
//! §Usage, cost, effort, sessions, limits).
//!
//! Adapters receive usage in slightly different JSON shapes (`input_tokens`
//! vs `prompt_tokens`, `cache_read_input_tokens` vs `cache_read_tokens`, …);
//! [`parse_usage`] folds the common spellings into one [`Usage`]. When a worker
//! reports no cost, the orchestrator asks a [`PriceTable`] (implemented by
//! `kevin-router`; [`ModelEntryPrices`] is the config-only fallback) through
//! [`finalize_cost`].

use std::collections::BTreeMap;

use kevin_domain::ModelAlias;
use rust_decimal::Decimal;
use serde_json::Value;

use crate::types::{ModelEntry, Usage};

/// Cost lookup for aliases whose worker does not report cost.
///
/// `kevin-router::PriceTable` implements this over the model catalog; tests
/// use [`NoPrices`] or [`ModelEntryPrices`].
pub trait PriceTable: Send + Sync {
    /// Cost in USD of `usage` under `alias`, `None` when the alias has no prices.
    fn cost(&self, alias: &ModelAlias, usage: &Usage) -> Option<Decimal>;
}

/// A price table that knows nothing (cost stays `None`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoPrices;

impl PriceTable for NoPrices {
    fn cost(&self, _alias: &ModelAlias, _usage: &Usage) -> Option<Decimal> {
        None
    }
}

/// Price table derived from `[models.*].input_usd_per_m` / `output_usd_per_m`.
#[derive(Debug, Clone, Default)]
pub struct ModelEntryPrices {
    entries: BTreeMap<ModelAlias, ModelEntry>,
}

impl ModelEntryPrices {
    /// Builds the table from a model catalog.
    #[must_use]
    pub fn new(entries: BTreeMap<ModelAlias, ModelEntry>) -> Self {
        Self { entries }
    }

    /// Cost of `usage` under `entry`, `None` when a price is missing.
    #[must_use]
    pub fn cost_for_entry(entry: &ModelEntry, usage: &Usage) -> Option<Decimal> {
        let input = entry.input_usd_per_m?;
        let output = entry.output_usd_per_m?;
        let per_m = Decimal::from(1_000_000u64);
        let input_tokens = Decimal::from(usage.input_tokens + usage.cache_read_tokens)
            + Decimal::from(usage.cache_write_tokens);
        let cost =
            input * input_tokens / per_m + output * Decimal::from(usage.output_tokens) / per_m;
        Some(cost.round_dp(6))
    }
}

impl PriceTable for ModelEntryPrices {
    fn cost(&self, alias: &ModelAlias, usage: &Usage) -> Option<Decimal> {
        self.entries
            .get(alias)
            .and_then(|entry| Self::cost_for_entry(entry, usage))
    }
}

/// Fills `usage.cost_usd` from `table` when the worker reported none.
pub fn finalize_cost(usage: &mut Usage, alias: &ModelAlias, table: &dyn PriceTable) {
    if usage.cost_usd.is_none() {
        usage.cost_usd = table.cost(alias, usage);
    }
}

fn get_u64(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| value.get(k))
        .and_then(|v| {
            v.as_u64().or_else(|| {
                v.as_f64()
                    .filter(|f| f.is_finite() && *f >= 0.0)
                    .and_then(|f| Decimal::from_f64_retain(f.round()))
                    .and_then(|d| u64::try_from(d).ok())
            })
        })
        .unwrap_or(0)
}

fn get_cost(value: &Value, keys: &[&str]) -> Option<Decimal> {
    keys.iter()
        .find_map(|k| value.get(k))
        .and_then(|v| match v {
            Value::Number(n) => n
                .as_f64()
                .and_then(Decimal::from_f64_retain)
                .map(|d| d.round_dp(8)),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
}

/// Normalises a usage JSON object from any adapter into a [`Usage`].
///
/// Recognised keys (first match wins):
/// - input: `input_tokens`, `prompt_tokens`, `input`
/// - output: `output_tokens`, `completion_tokens`, `output`
/// - cache read: `cache_read_input_tokens`, `cache_read_tokens`, `cached_input_tokens`, `cache_read`
/// - cache write: `cache_creation_input_tokens`, `cache_write_tokens`, `cache_creation_tokens`, `cache_write`
/// - cost: `total_cost_usd`, `cost_usd`, `cost`
/// - wall: `duration_ms`, `wall_ms`
///
/// Missing keys are zero / `None`; a non-object yields an empty usage.
#[must_use]
pub fn parse_usage(value: &Value) -> Usage {
    if !value.is_object() {
        return Usage::default();
    }
    Usage {
        input_tokens: get_u64(value, &["input_tokens", "prompt_tokens", "input"]),
        output_tokens: get_u64(value, &["output_tokens", "completion_tokens", "output"]),
        cache_read_tokens: get_u64(
            value,
            &[
                "cache_read_input_tokens",
                "cache_read_tokens",
                "cached_input_tokens",
                "cache_read",
            ],
        ),
        cache_write_tokens: get_u64(
            value,
            &[
                "cache_creation_input_tokens",
                "cache_write_tokens",
                "cache_creation_tokens",
                "cache_write",
            ],
        ),
        cost_usd: get_cost(value, &["total_cost_usd", "cost_usd", "cost"]),
        wall_ms: get_u64(value, &["duration_ms", "wall_ms"]),
    }
}

/// Sums a sequence of usages.
pub fn sum<I: IntoIterator<Item = Usage>>(usages: I) -> Usage {
    usages.into_iter().fold(Usage::default(), |acc, u| acc + u)
}

#[cfg(test)]
mod tests {
    use kevin_domain::WorkerKind;
    use serde_json::json;

    use super::*;

    #[test]
    fn parse_usage_accepts_claude_and_codex_spellings() {
        let claude = json!({
            "input_tokens": 12, "output_tokens": 7,
            "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5,
            "total_cost_usd": 0.0123, "duration_ms": 1500
        });
        let u = parse_usage(&claude);
        assert_eq!(u.input_tokens, 12);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 100);
        assert_eq!(u.cache_write_tokens, 5);
        assert_eq!(u.cost_usd, Some(Decimal::new(123, 4)));
        assert_eq!(u.wall_ms, 1500);

        let codex = json!({"prompt_tokens": 3, "completion_tokens": 4, "cached_input_tokens": 1});
        let u = parse_usage(&codex);
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (3, 4, 1)
        );
        assert_eq!(u.cost_usd, None);
        assert!(parse_usage(&json!("nope")).is_empty());
        assert_eq!(
            parse_usage(&json!({"cost_usd": "0.5"})).cost_usd,
            Some(Decimal::new(5, 1))
        );
    }

    #[test]
    fn price_table_fallback_fills_missing_cost_only() {
        let alias = ModelAlias::new("sonnet5-claude").unwrap();
        let entry = ModelEntry::new(WorkerKind::Claude, "claude-sonnet-5").price_cents(300, 1500);
        let table = ModelEntryPrices::new(BTreeMap::from([(alias.clone(), entry)]));
        let mut usage = Usage::tokens(1_000_000, 1_000_000);
        finalize_cost(&mut usage, &alias, &table);
        assert_eq!(usage.cost_usd, Some(Decimal::new(18, 0)));

        let mut reported = Usage {
            cost_usd: Some(Decimal::new(1, 0)),
            ..Usage::tokens(10, 10)
        };
        finalize_cost(&mut reported, &alias, &table);
        assert_eq!(reported.cost_usd, Some(Decimal::new(1, 0)));

        let unknown = ModelAlias::new("gpt56-codex").unwrap();
        let mut usage = Usage::tokens(10, 10);
        finalize_cost(&mut usage, &unknown, &table);
        assert_eq!(usage.cost_usd, None);
        finalize_cost(&mut usage, &alias, &NoPrices);
        assert_eq!(usage.cost_usd, None);
    }

    #[test]
    fn sum_adds_everything() {
        let total = sum([Usage::tokens(1, 2), Usage::tokens(3, 4)]);
        assert_eq!(total, Usage::tokens(4, 6));
    }
}
