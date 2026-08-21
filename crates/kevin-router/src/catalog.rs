//! Model catalog (`plan/06-memory-and-learning.md` §2.1).
//!
//! [`ModelCatalog::from_config`] materialises `[models]` into the routing
//! vocabulary: alias → `(worker, model, tier, prices, tags, enabled)`. Its
//! [`catalog_version`](ModelCatalog::version) is the sha256 of the canonical
//! TOML of `[models]`, so the same catalog always hashes the same and any
//! catalog edit produces a new version. Every `task.routed` event and every
//! `routing.route_outcomes` row records it, which keeps leaderboards readable
//! across config edits.

use std::collections::BTreeMap;

use kevin_config::{KevinConfig, ModelEntry};
use kevin_domain::route_score::BetaPrior;
use kevin_domain::{Decimal, ModelAlias, Tier, WorkerKind};
use sha2::{Digest, Sha256};

use crate::price::PriceTable;

/// Maps the config tier onto the domain tier.
///
// TODO(ws-01/ws-02): `kevin_config::Tier` and `kevin_domain::Tier` are the same
// enum declared twice (WS-02 landed before WS-01 moved it into the domain);
// once config re-exports the domain type this mapping disappears.
#[must_use]
pub const fn domain_tier(tier: kevin_config::Tier) -> Tier {
    match tier {
        kevin_config::Tier::Fast => Tier::Fast,
        kevin_config::Tier::Balanced => Tier::Balanced,
        kevin_config::Tier::Frontier => Tier::Frontier,
    }
}

/// Quality prior per tier when a route has no judged outcome yet
/// (`plan/06-memory-and-learning.md` §2.3).
#[must_use]
pub const fn tier_quality_prior(tier: Tier) -> f32 {
    match tier {
        Tier::Frontier => 0.80,
        Tier::Balanced => 0.70,
        Tier::Fast => 0.55,
    }
}

/// One catalog entry: a `[models.<alias>]` block plus whether its worker is
/// enabled in this configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    /// The alias itself.
    pub alias: ModelAlias,
    /// Which adapter runs it.
    pub worker: WorkerKind,
    /// Provider model id.
    pub model: String,
    /// Capability tier.
    pub tier: Tier,
    /// Context window when known.
    pub context_tokens: Option<u64>,
    /// USD per 1M input tokens.
    pub input_usd_per_m: Option<Decimal>,
    /// USD per 1M output tokens.
    pub output_usd_per_m: Option<Decimal>,
    /// Capability tags.
    pub tags: Vec<String>,
    /// Worker-specific extras (e.g. pi's `provider`).
    pub extra: BTreeMap<String, toml::Value>,
    /// `[workers.<worker>].enabled`.
    pub enabled: bool,
}

impl CatalogEntry {
    fn from_entry(alias: &ModelAlias, entry: &ModelEntry, enabled: bool) -> Self {
        Self {
            alias: alias.clone(),
            worker: entry.worker,
            model: entry.model.clone(),
            tier: domain_tier(entry.tier),
            context_tokens: entry.context_tokens,
            input_usd_per_m: entry.input_usd_per_m,
            output_usd_per_m: entry.output_usd_per_m,
            tags: entry.tags.clone(),
            extra: entry.extra.clone(),
            enabled,
        }
    }

    /// Cold-start Beta prior for this entry's tier.
    #[must_use]
    pub const fn prior(&self) -> BetaPrior {
        BetaPrior::for_tier(self.tier)
    }

    /// Quality prior for this entry's tier.
    #[must_use]
    pub const fn quality_prior(&self) -> f32 {
        tier_quality_prior(self.tier)
    }

    /// Whether the entry carries `tag`.
    #[must_use]
    pub fn has_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|t| t == tag)
    }
}

/// The routing vocabulary: every `[models.<alias>]` entry plus a version hash.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCatalog {
    entries: BTreeMap<ModelAlias, CatalogEntry>,
    version: String,
    prices: PriceTable,
}

impl ModelCatalog {
    /// Materialises `[models]`, marking each alias with whether its worker is
    /// enabled (`[workers.<kind>].enabled`).
    #[must_use]
    pub fn from_config(config: &KevinConfig) -> Self {
        let entries = config
            .models
            .iter()
            .map(|(alias, entry)| {
                (
                    alias.clone(),
                    CatalogEntry::from_entry(alias, entry, config.workers.is_enabled(entry.worker)),
                )
            })
            .collect();
        Self {
            entries,
            version: catalog_version(&config.models),
            prices: PriceTable::from_models(&config.models),
        }
    }

    /// The catalog version: sha256 (hex) of the canonical TOML of `[models]`.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The entry of `alias`, if it exists.
    #[must_use]
    pub fn get(&self, alias: &ModelAlias) -> Option<&CatalogEntry> {
        self.entries.get(alias)
    }

    /// Whether `alias` exists and its worker is enabled.
    #[must_use]
    pub fn is_usable(&self, alias: &ModelAlias) -> bool {
        self.entries.get(alias).is_some_and(|e| e.enabled)
    }

    /// Every entry, alias order.
    pub fn entries(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.values()
    }

    /// Entries whose worker is enabled, alias order.
    pub fn usable(&self) -> impl Iterator<Item = &CatalogEntry> {
        self.entries.values().filter(|e| e.enabled)
    }

    /// Usable entries carrying `tag`.
    pub fn tagged<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a CatalogEntry> {
        self.usable().filter(move |e| e.has_tag(tag))
    }

    /// Cold-start prior of `alias` (balanced when the alias is unknown).
    #[must_use]
    pub fn prior_for(&self, alias: &ModelAlias) -> BetaPrior {
        self.get(alias)
            .map_or_else(|| BetaPrior::for_tier(Tier::Balanced), CatalogEntry::prior)
    }

    /// Quality prior of `alias` (balanced when the alias is unknown).
    #[must_use]
    pub fn quality_prior_for(&self, alias: &ModelAlias) -> f32 {
        self.get(alias).map_or_else(
            || tier_quality_prior(Tier::Balanced),
            CatalogEntry::quality_prior,
        )
    }

    /// The price table of this catalog.
    #[must_use]
    pub fn prices(&self) -> &PriceTable {
        &self.prices
    }

    /// Number of aliases.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the catalog has no alias at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// sha256 (hex) of the canonical TOML of `[models]`.
///
/// Canonical = `toml` serialisation of the `BTreeMap`, i.e. aliases in sorted
/// order with their fields in schema order; formatting of the source file,
/// comments and key order therefore never affect the version.
#[must_use]
pub fn catalog_version(models: &BTreeMap<ModelAlias, ModelEntry>) -> String {
    let canonical = toml::to_string(models)
        .unwrap_or_else(|e| unreachable!("[models] serializes to TOML: {e}"));
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use kevin_config::schema::default_models;

    use super::*;

    fn config() -> KevinConfig {
        KevinConfig::default()
    }

    fn alias(name: &str) -> ModelAlias {
        ModelAlias::new(name).unwrap()
    }

    #[test]
    fn from_config_marks_disabled_workers() {
        let catalog = ModelCatalog::from_config(&config());
        // `[workers.fake].enabled` defaults to false.
        assert!(!catalog.get(&alias("fake")).unwrap().enabled);
        assert!(catalog.get(&alias("sonnet5-claude")).unwrap().enabled);
        assert!(!catalog.is_usable(&alias("fake")));
        assert!(catalog.usable().all(|e| e.worker != WorkerKind::Fake));
    }

    #[test]
    fn catalog_version_is_stable_and_content_addressed() {
        let a = catalog_version(&default_models());
        let b = catalog_version(&default_models());
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);

        let mut changed = default_models();
        changed
            .get_mut(&alias("sonnet5-claude"))
            .unwrap()
            .tags
            .push("extra".to_owned());
        assert_ne!(catalog_version(&changed), a);
    }

    #[test]
    fn priors_follow_the_tier_table() {
        let catalog = ModelCatalog::from_config(&config());
        assert_eq!(
            catalog.prior_for(&alias("opus5-claude")),
            BetaPrior {
                alpha: 3.0,
                beta: 1.0
            }
        );
        assert_eq!(
            catalog.prior_for(&alias("sonnet5-claude")),
            BetaPrior {
                alpha: 2.0,
                beta: 1.0
            }
        );
        assert_eq!(
            catalog.prior_for(&alias("haiku45-claude")),
            BetaPrior {
                alpha: 1.5,
                beta: 1.5
            }
        );
        assert!((catalog.quality_prior_for(&alias("opus5-claude")) - 0.80).abs() < f32::EPSILON);
        assert!((catalog.quality_prior_for(&alias("haiku45-claude")) - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn tagged_lists_usable_entries_only() {
        let catalog = ModelCatalog::from_config(&config());
        let judges: Vec<_> = catalog
            .tagged("judge")
            .map(|e| e.alias.to_string())
            .collect();
        assert_eq!(judges, vec!["fable5-claude", "opus5-claude"]);
    }
}
