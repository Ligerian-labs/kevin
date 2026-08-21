//! Cost model (`plan/06-memory-and-learning.md` §2.1, `plan/03-config-schema.md`
//! §model catalog).
//!
//! Prices are USD per 1M tokens and come from `[models.<alias>]`. An alias
//! without both prices has **unknown** cost: [`PriceTable::cost`] returns
//! `None` and every downstream number (leaderboard `$/task`,
//! `routing.route_outcomes.cost_usd`) stays null rather than pretending zero.

use std::collections::BTreeMap;

use kevin_config::{KevinConfig, ModelEntry};
use kevin_domain::{Decimal, ModelAlias, Usage};

/// What the price table needs to know about an attempt's token usage.
/// Implemented by [`kevin_domain::Usage`].
pub trait UsageLike {
    /// Prompt tokens.
    fn input_tokens(&self) -> u64;
    /// Completion tokens.
    fn output_tokens(&self) -> u64;
    /// Tokens served from the prompt cache (billed at the input price).
    fn cache_read_tokens(&self) -> u64 {
        0
    }
    /// Tokens written to the prompt cache (billed at the input price).
    fn cache_write_tokens(&self) -> u64 {
        0
    }
    /// Cost the worker itself reported, when it reported one.
    fn reported_cost_usd(&self) -> Option<Decimal> {
        None
    }
}

impl UsageLike for Usage {
    fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    fn output_tokens(&self) -> u64 {
        self.output_tokens
    }

    fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }

    fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }

    fn reported_cost_usd(&self) -> Option<Decimal> {
        self.cost_usd
    }
}

impl<T: UsageLike + ?Sized> UsageLike for &T {
    fn input_tokens(&self) -> u64 {
        (**self).input_tokens()
    }

    fn output_tokens(&self) -> u64 {
        (**self).output_tokens()
    }

    fn cache_read_tokens(&self) -> u64 {
        (**self).cache_read_tokens()
    }

    fn cache_write_tokens(&self) -> u64 {
        (**self).cache_write_tokens()
    }

    fn reported_cost_usd(&self) -> Option<Decimal> {
        (**self).reported_cost_usd()
    }
}

/// Tokens per price unit (prices are per 1M tokens).
pub const TOKENS_PER_PRICE_UNIT: u64 = 1_000_000;

/// Decimal places kept on a computed cost.
pub const COST_SCALE: u32 = 6;

/// Prices of one alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AliasPrice {
    /// USD per 1M input tokens.
    pub input_usd_per_m: Option<Decimal>,
    /// USD per 1M output tokens.
    pub output_usd_per_m: Option<Decimal>,
}

impl AliasPrice {
    /// Both prices are known.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.input_usd_per_m.is_some() && self.output_usd_per_m.is_some()
    }

    /// Prices of a config entry.
    #[must_use]
    pub const fn from_entry(entry: &ModelEntry) -> Self {
        Self {
            input_usd_per_m: entry.input_usd_per_m,
            output_usd_per_m: entry.output_usd_per_m,
        }
    }
}

/// Alias → prices, built from `[models]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriceTable {
    prices: BTreeMap<ModelAlias, AliasPrice>,
}

impl PriceTable {
    /// Builds the table from the `[models]` section.
    #[must_use]
    pub fn from_models(models: &BTreeMap<ModelAlias, ModelEntry>) -> Self {
        Self {
            prices: models
                .iter()
                .map(|(alias, entry)| (alias.clone(), AliasPrice::from_entry(entry)))
                .collect(),
        }
    }

    /// Builds the table from a whole configuration.
    #[must_use]
    pub fn from_config(config: &KevinConfig) -> Self {
        Self::from_models(&config.models)
    }

    /// Prices of `alias`, if the alias exists.
    #[must_use]
    pub fn prices(&self, alias: &ModelAlias) -> Option<AliasPrice> {
        self.prices.get(alias).copied()
    }

    /// Cost of `usage` on `alias` from the price table, or `None` when the
    /// alias is unknown or either price is unset.
    ///
    /// Cache read/write tokens are billed at the input price (the catalog has
    /// no separate cache prices); wall time is not billed.
    #[must_use]
    pub fn cost(&self, alias: &ModelAlias, usage: &impl UsageLike) -> Option<Decimal> {
        let price = self.prices.get(alias)?;
        let (input_price, output_price) = (price.input_usd_per_m?, price.output_usd_per_m?);
        let billable_input = usage
            .input_tokens()
            .saturating_add(usage.cache_read_tokens())
            .saturating_add(usage.cache_write_tokens());
        let per_unit = Decimal::from(TOKENS_PER_PRICE_UNIT);
        let input_cost = (Decimal::from(billable_input) * input_price).checked_div(per_unit)?;
        let output_cost =
            (Decimal::from(usage.output_tokens()) * output_price).checked_div(per_unit)?;
        Some((input_cost + output_cost).round_dp(COST_SCALE))
    }

    /// The cost Kevin accounts for: what the worker reported when it reported
    /// one, else [`PriceTable::cost`].
    #[must_use]
    pub fn effective_cost(&self, alias: &ModelAlias, usage: &impl UsageLike) -> Option<Decimal> {
        usage
            .reported_cost_usd()
            .or_else(|| self.cost(alias, usage))
    }

    /// Number of aliases in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.prices.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use kevin_config::schema::default_models;
    use rust_decimal::prelude::FromPrimitive;

    use super::*;

    fn alias(name: &str) -> ModelAlias {
        ModelAlias::new(name).unwrap()
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::ZERO
        }
    }

    #[test]
    fn cost_uses_input_and_output_prices() {
        let table = PriceTable::from_models(&default_models());
        // sonnet5-claude: $3/M input, $15/M output.
        assert_eq!(
            table.cost(&alias("sonnet5-claude"), &usage(1_000_000, 100_000)),
            Some(Decimal::from_f64(4.5).unwrap())
        );
    }

    #[test]
    fn cache_tokens_are_billed_at_the_input_price() {
        let table = PriceTable::from_models(&default_models());
        let usage = Usage {
            input_tokens: 500_000,
            cache_read_tokens: 300_000,
            cache_write_tokens: 200_000,
            ..Usage::ZERO
        };
        assert_eq!(
            table.cost(&alias("sonnet5-claude"), &usage),
            Some(Decimal::from(3))
        );
    }

    #[test]
    fn cost_is_null_without_prices_or_alias() {
        let table = PriceTable::from_models(&default_models());
        let usage = usage(1_000_000, 1_000_000);
        assert_eq!(table.cost(&alias("gpt56-codex"), &usage), None);
        assert_eq!(table.cost(&alias("nope"), &usage), None);
        assert!(!table.prices(&alias("gpt56-codex")).unwrap().is_known());
    }

    #[test]
    fn effective_cost_prefers_the_reported_cost() {
        let table = PriceTable::from_models(&default_models());
        let reported = Decimal::from_f64(0.25).unwrap();
        let usage = Usage {
            cost_usd: Some(reported),
            ..usage(1_000_000, 0)
        };
        assert_eq!(
            table.effective_cost(&alias("sonnet5-claude"), &usage),
            Some(reported)
        );
        assert_eq!(
            table.effective_cost(&alias("gpt56-codex"), &usage),
            Some(reported)
        );
    }
}
