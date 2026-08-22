//! `GET /v1/kohral/models` — the runtime model catalog v1
//! (`plan/08-kohral-runtime.md` §1.5).
//!
//! Kohral's model picker for a Kevin agent is fed from this endpoint
//! (`ModelCatalog::fetchRuntimeCatalog`, verified against
//! `kohral src/AgentRuntime/Application/ModelCatalog.php`: it reads `id`,
//! `name` and `capabilities` per model, ignores every other key, and rejects
//! the whole payload unless `object` / `version` / `providers` are exactly the
//! v1 envelope).
//!
//! The catalog is **derived from `[models.*]`**, never hand-maintained: an
//! alias is one `(provider, model)` pair, and the reverse direction
//! ([`RuntimeCatalog::resolve`]) turns the `"<provider>/<model>"` a Kohral
//! operator picks back into the alias Kevin routes with. Only aliases whose worker is
//! authenticated are listed, exactly as Hermes lists authenticated providers
//! only — a model the agent cannot actually call has no business in a picker.

use std::collections::BTreeMap;

use kevin_domain::{ModelAlias, WorkerKind};
use kevin_router::{CatalogEntry, ModelCatalog, Tier};
use serde_json::{Value, json};

/// Kohral drops a model id longer than this.
const MAX_MODEL_ID: usize = 255;
/// Kohral stops reading after this many models.
const MAX_MODELS: usize = 2000;
/// Kohral truncates a provider name here.
const MAX_PROVIDER_NAME: usize = 120;

/// The `model` value Kohral sends when the operator picked no override.
pub const DEFAULT_MODEL: &str = "hermes-agent";

/// One provider row of the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// Slug (`[a-z0-9][a-z0-9._-]*`).
    pub id: String,
    /// Human label.
    pub name: String,
    /// Models, alias order.
    pub models: Vec<Model>,
}

/// One model of a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// Provider-side model id (`[A-Za-z0-9][A-Za-z0-9._:/-]*`).
    pub id: String,
    /// The Kevin alias behind it — extra keys are ignored by Kohral, and it
    /// makes the mapping visible to an operator reading the JSON.
    pub alias: ModelAlias,
    /// Kevin's alias name, shown in the picker.
    pub name: String,
    /// `["reasoning"]` for the frontier and balanced tiers; Kohral adds
    /// `"tools"` itself.
    pub capabilities: Vec<String>,
}

/// The full catalog document, plus the reverse index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeCatalog {
    providers: Vec<Provider>,
    by_pair: BTreeMap<(String, String), ModelAlias>,
}

impl RuntimeCatalog {
    /// Builds the catalog from `[models.*]`, keeping only aliases whose worker
    /// `authenticated` reports as usable.
    #[must_use]
    pub fn build(catalog: &ModelCatalog, authenticated: &dyn Fn(WorkerKind) -> bool) -> Self {
        let mut providers: Vec<Provider> = Vec::new();
        let mut by_pair = BTreeMap::new();
        let mut count = 0usize;

        for entry in catalog.entries() {
            if !entry.enabled || !authenticated(entry.worker) || count >= MAX_MODELS {
                continue;
            }
            let Some((provider_id, model_id)) = split(entry) else {
                continue;
            };
            if !valid_provider(&provider_id) || !valid_model(&model_id) {
                tracing::debug!(
                    alias = entry.alias.as_str(),
                    provider = provider_id,
                    model = model_id,
                    "alias skipped: Kohral would reject the provider or model id"
                );
                continue;
            }
            let model = Model {
                id: model_id.clone(),
                alias: entry.alias.clone(),
                name: entry.alias.as_str().to_owned(),
                capabilities: capabilities(entry.tier),
            };
            by_pair
                .entry((provider_id.clone(), model_id))
                .or_insert_with(|| entry.alias.clone());
            match providers.iter_mut().find(|p| p.id == provider_id) {
                Some(provider) => provider.models.push(model),
                None => providers.push(Provider {
                    name: provider_name(&provider_id, entry.worker),
                    id: provider_id,
                    models: vec![model],
                }),
            }
            count += 1;
        }

        Self { providers, by_pair }
    }

    /// The providers, in first-seen order.
    #[must_use]
    pub fn providers(&self) -> &[Provider] {
        &self.providers
    }

    /// Total number of models.
    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.iter().map(|p| p.models.len()).sum()
    }

    /// Whether nothing is offered (no authenticated worker).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The v1 envelope Kohral parses.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "object": "kohral.runtime_model_catalog",
            "version": 1,
            "providers": self
                .providers
                .iter()
                .map(|provider| json!({
                    "id": provider.id,
                    "name": provider.name,
                    "models": provider
                        .models
                        .iter()
                        .map(|model| json!({
                            "id": model.id,
                            "name": model.name,
                            "alias": model.alias.as_str(),
                            "capabilities": model.capabilities,
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Turns the `model` field of a turn into the alias Kevin routes with.
    ///
    /// - `""` / `"hermes-agent"` → `None`: no override, the configured roles win.
    /// - `"<provider>/<model>"` → the first alias with that pair.
    /// - a bare Kevin alias is accepted too, because an operator editing the
    ///   native configuration thinks in aliases (`roles.planner = "…"`).
    #[must_use]
    pub fn resolve(&self, model: &str) -> Resolution {
        let model = model.trim();
        if model.is_empty() || model == DEFAULT_MODEL {
            return Resolution::NoOverride;
        }
        if let Some((provider, name)) = model.split_once('/')
            && let Some(alias) = self.by_pair.get(&(provider.to_owned(), name.to_owned()))
        {
            return Resolution::Alias(alias.clone());
        }
        if let Ok(alias) = ModelAlias::new(model)
            && self.by_pair.values().any(|known| *known == alias)
        {
            return Resolution::Alias(alias);
        }
        Resolution::Unknown
    }
}

/// What [`RuntimeCatalog::resolve`] made of a `model` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Keep the configured roles.
    NoOverride,
    /// Override the planner/judge/default roles with this alias.
    Alias(ModelAlias),
    /// `400 unknown_model`.
    Unknown,
}

/// `(provider, model)` for an alias.
///
/// `provider` comes from the alias' own `provider` key when it has one (pi
/// declares it), otherwise from the worker: `claude → anthropic`,
/// `codex → openai`, `fake → fake`, and for `opencode` from the
/// `provider/model` prefix its model ids carry.
fn split(entry: &CatalogEntry) -> Option<(String, String)> {
    if let Some(provider) = entry.extra.get("provider").and_then(toml::Value::as_str) {
        return Some((
            provider.trim().to_ascii_lowercase(),
            strip_prefix(&entry.model, provider),
        ));
    }
    match entry.worker {
        WorkerKind::Claude => Some(("anthropic".to_owned(), entry.model.clone())),
        WorkerKind::Codex => Some(("openai".to_owned(), entry.model.clone())),
        WorkerKind::Fake => Some(("fake".to_owned(), entry.model.clone())),
        WorkerKind::Pi | WorkerKind::Opencode => {
            let (provider, model) = entry.model.split_once('/')?;
            Some((provider.trim().to_ascii_lowercase(), model.to_owned()))
        }
    }
}

fn strip_prefix(model: &str, provider: &str) -> String {
    model
        .strip_prefix(provider)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(model)
        .to_owned()
}

fn provider_name(id: &str, worker: WorkerKind) -> String {
    let label = match id {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "google" => "Google",
        "openrouter" => "OpenRouter",
        "fake" => "Kevin fake worker",
        other => other,
    };
    let name = if worker == WorkerKind::Fake {
        label.to_owned()
    } else {
        format!("{label} (via {} CLI)", worker.as_str())
    };
    name.chars().take(MAX_PROVIDER_NAME).collect()
}

/// Kohral adds `"tools"`; Kevin only declares whether the model reasons.
fn capabilities(tier: Tier) -> Vec<String> {
    match tier {
        Tier::Frontier | Tier::Balanced => vec!["reasoning".to_owned()],
        Tier::Fast => Vec::new(),
    }
}

fn valid_provider(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn valid_model(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_MODEL_ID {
        return false;
    }
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'))
}

#[cfg(test)]
mod tests {
    use kevin_config::{KevinConfig, ModelEntry};
    use kevin_domain::{ModelAlias, WorkerKind};
    use kevin_router::ModelCatalog;

    use super::{DEFAULT_MODEL, Resolution, RuntimeCatalog};

    fn alias(name: &str) -> ModelAlias {
        ModelAlias::new(name).expect("valid alias")
    }

    fn config() -> KevinConfig {
        let mut config = KevinConfig::default();
        config.workers.claude.enabled = true;
        config.workers.codex.enabled = true;
        config.workers.pi.enabled = true;
        config.workers.opencode.enabled = true;
        config.workers.fake.enabled = true;
        config
    }

    fn catalog(config: &KevinConfig, authenticated: &[WorkerKind]) -> RuntimeCatalog {
        let owned = authenticated.to_vec();
        RuntimeCatalog::build(&ModelCatalog::from_config(config), &move |kind| {
            owned.contains(&kind)
        })
    }

    #[test]
    fn ac_ws22_6_the_catalog_is_derived_from_the_model_aliases() {
        let catalog = catalog(&config(), &WorkerKind::ALL);
        let document = catalog.to_json();
        assert_eq!(document["object"], "kohral.runtime_model_catalog");
        assert_eq!(document["version"], 1);

        let anthropic = catalog
            .providers()
            .iter()
            .find(|provider| provider.id == "anthropic")
            .expect("the default catalog has claude aliases");
        assert_eq!(anthropic.name, "Anthropic (via claude CLI)");
        let ids: Vec<&str> = anthropic.models.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-opus-5"), "{ids:?}");
        let opus = anthropic
            .models
            .iter()
            .find(|m| m.id == "claude-opus-5")
            .expect("opus alias");
        assert_eq!(opus.name, "opus5-claude");
        assert!(opus.capabilities.contains(&"reasoning".to_owned()));

        assert!(
            catalog.providers().iter().any(|p| p.id == "openai"),
            "codex aliases become the openai provider"
        );
    }

    #[test]
    fn a_pi_alias_uses_its_declared_provider() {
        let mut config = config();
        config.models.insert(
            alias("sonnet5-pi-test"),
            ModelEntry::new(WorkerKind::Pi, "anthropic/claude-sonnet-5")
                .extra("provider", "anthropic"),
        );
        let catalog = catalog(&config, &[WorkerKind::Pi]);
        let anthropic = catalog
            .providers()
            .iter()
            .find(|p| p.id == "anthropic")
            .expect("provider");
        assert!(
            anthropic.models.iter().any(|m| m.id == "claude-sonnet-5"),
            "the provider prefix is stripped from the model id: {:?}",
            anthropic.models
        );
    }

    #[test]
    fn an_opencode_alias_splits_provider_from_model() {
        let mut config = KevinConfig::default();
        config.workers.opencode.enabled = true;
        config.models.clear();
        config.models.insert(
            alias("oc"),
            ModelEntry::new(WorkerKind::Opencode, "anthropic/claude-sonnet-5"),
        );
        let catalog = catalog(&config, &[WorkerKind::Opencode]);
        assert_eq!(catalog.providers().len(), 1);
        assert_eq!(catalog.providers()[0].id, "anthropic");
        assert_eq!(catalog.providers()[0].models[0].id, "claude-sonnet-5");
    }

    #[test]
    fn only_authenticated_workers_are_listed() {
        let catalog = catalog(&config(), &[WorkerKind::Claude]);
        assert!(catalog.providers().iter().all(|p| p.id == "anthropic"));
        assert!(!catalog.is_empty());

        let none = catalog2(&config(), &[]);
        assert!(none.is_empty(), "no authenticated worker → no models");
        assert_eq!(
            none.to_json()["providers"].as_array().expect("array").len(),
            0
        );
    }

    fn catalog2(config: &KevinConfig, authenticated: &[WorkerKind]) -> RuntimeCatalog {
        catalog(config, authenticated)
    }

    #[test]
    fn a_disabled_worker_is_never_listed_even_when_authenticated() {
        let mut config = config();
        config.workers.codex.enabled = false;
        let catalog = catalog(&config, &WorkerKind::ALL);
        assert!(
            !catalog.providers().iter().any(|p| p.id == "openai"),
            "codex is disabled in [workers], so its aliases are not usable"
        );
    }

    #[test]
    fn the_model_field_resolves_back_to_an_alias() {
        let catalog = catalog(&config(), &WorkerKind::ALL);
        assert_eq!(catalog.resolve(DEFAULT_MODEL), Resolution::NoOverride);
        assert_eq!(catalog.resolve(""), Resolution::NoOverride);
        assert_eq!(catalog.resolve("   "), Resolution::NoOverride);
        assert_eq!(
            catalog.resolve("anthropic/claude-opus-5"),
            Resolution::Alias(alias("opus5-claude"))
        );
        assert_eq!(
            catalog.resolve("opus5-claude"),
            Resolution::Alias(alias("opus5-claude")),
            "a bare Kevin alias is accepted too"
        );
        assert_eq!(catalog.resolve("anthropic/nope"), Resolution::Unknown);
        assert_eq!(catalog.resolve("not a model"), Resolution::Unknown);
    }

    #[test]
    fn every_id_matches_the_pattern_kohral_enforces() {
        let provider_re = regex::Regex::new("^[a-z0-9][a-z0-9._-]*$").expect("regex");
        let model_re = regex::Regex::new("^[A-Za-z0-9][A-Za-z0-9._:/-]*$").expect("regex");
        let catalog = catalog(&config(), &WorkerKind::ALL);
        assert!(!catalog.is_empty());
        for provider in catalog.providers() {
            assert!(provider_re.is_match(&provider.id), "{}", provider.id);
            assert!(provider.name.chars().count() <= 120);
            for model in &provider.models {
                assert!(model_re.is_match(&model.id), "{}", model.id);
                assert!(model.id.len() <= 255);
            }
        }
    }
}
