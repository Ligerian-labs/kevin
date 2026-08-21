//! Configuration context (`plan/03-config-schema.md`).
//!
//! Owns the typed [`KevinConfig`] schema (TOML, `deny_unknown_fields`), the
//! layered loader (defaults → user file → project file → `--config` file →
//! environment → `--set`), whole-config validation with aggregated errors,
//! redaction for `kevin config show`, and the default model catalog.
//!
//! Dependency direction: depends on `kevin-domain` only (for value objects such
//! as `ModelAlias`). Every other crate may depend on it; it depends on none of
//! them.
//!
//! ```
//! use kevin_config::{LoadOptions, load};
//!
//! let resolved = load(LoadOptions::hermetic().set("kevin.profile=server")).unwrap();
//! assert!(!resolved.config.database.auto_migrate); // flipped by the server profile
//! assert_eq!(resolved.source_of("server.docs").to_string(), "profile:server");
//! ```

use std::collections::BTreeMap;

pub mod duration;
pub mod error;
pub mod loader;
pub mod paths;
pub mod redact;
pub mod schema;
pub mod source;
pub mod token;
pub mod validate;

pub use error::{ConfigError, ConfigErrors};
pub use loader::{find_project_file, load, profile_overrides};
pub use schema::*;
pub use source::{LoadOptions, Source};

/// The TOML block from `plan/03-config-schema.md`, byte for byte; parsing it
/// yields exactly `KevinConfig::default()`, and `kevin config init` writes it.
pub const DEFAULT_TOML: &str = include_str!("../assets/default.toml");

/// Per-leaf-key provenance: dotted key path → [`Source`].
pub type Sources = BTreeMap<String, Source>;

/// The effective configuration plus where every value came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The validated, immutable configuration.
    pub config: KevinConfig,
    /// Provenance of every leaf key (`kevin.profile`, `models.fake.tier`, …).
    pub sources: Sources,
}

impl Resolved {
    /// Where `key` (a dotted path) came from; [`Source::Unknown`] for unknown keys.
    #[must_use]
    pub fn source_of(&self, key: &str) -> Source {
        self.sources.get(key).cloned().unwrap_or(Source::Unknown)
    }

    /// The effective configuration as a TOML table with secrets redacted.
    #[must_use]
    pub fn redacted_table(&self) -> toml::Table {
        let mut table = toml::Table::try_from(&self.config)
            .unwrap_or_else(|e| unreachable!("KevinConfig serializes to TOML: {e}"));
        redact::redact_table(&mut table);
        table
    }

    /// The effective configuration as TOML with secrets redacted
    /// (`*token*`/`*key*`/`*identity*` leaves → `***`, URL passwords masked).
    #[must_use]
    pub fn redacted_toml(&self) -> String {
        toml::to_string_pretty(&self.redacted_table())
            .unwrap_or_else(|e| unreachable!("TOML table serializes: {e}"))
    }

    /// Redacted config as `key.path = value  # source` lines (`config show --sources`).
    #[must_use]
    pub fn redacted_toml_with_sources(&self) -> String {
        redact::render_with_sources(&self.redacted_table(), &self.sources)
    }

    /// Redacted config and sources as JSON (`--json` / `GET /api/v1/config`):
    /// `{ "config": {...}, "sources": { "kevin.profile": "default", ... } }`.
    #[must_use]
    pub fn redacted_json(&self) -> serde_json::Value {
        let sources: BTreeMap<&str, String> = self
            .sources
            .iter()
            .map(|(k, v)| (k.as_str(), v.to_string()))
            .collect();
        serde_json::json!({ "config": self.redacted_table(), "sources": sources })
    }
}
