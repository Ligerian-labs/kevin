//! Layered loading (`plan/03-config-schema.md` §Sources and precedence).
//!
//! Layers are merged as `toml::Table`s (tables deep-merge, everything else
//! replaces) while a per-leaf-key source map is maintained. After every layer
//! the cumulative table is deserialized into [`KevinConfig`]; a failure is
//! attributed to that layer (key path via `serde_path_to_error`) and the layer
//! is reverted so later layers are still checked. Profile defaults are applied
//! last, only to keys still at their built-in default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::error::{ConfigError, ConfigErrors};
use crate::paths;
use crate::schema::{KevinConfig, Profile, SandboxTier};
use crate::source::{ENV_PREFIX, KEVIN_CONFIG_ENV, KOHRAL_ENV_ALIASES, LoadOptions, Source};
use crate::validate;
use crate::{Resolved, Sources};

/// Top-level sections a project-layer file may not touch.
pub const PROJECT_PROTECTED_SECTIONS: &[&str] =
    &["sandbox", "workers", "server", "database", "kohral"];

/// Relative path of the project config file.
pub const PROJECT_FILE: &str = ".kevin/kevin.toml";

/// Key of the derived `sandbox.allow_dangerous_flags`.
const ALLOW_DANGEROUS_KEY: &str = "sandbox.allow_dangerous_flags";

/// Loads, layers, and validates the configuration.
pub fn load(opts: LoadOptions) -> Result<Resolved, ConfigErrors> {
    let LoadOptions {
        user_file,
        project_dir,
        config_file,
        env,
        sets,
    } = opts;
    let mut errors = ConfigErrors::default();
    let mut state = Layered::defaults();

    // 2. user file (missing → skipped)
    if let Some(path) = &user_file
        && path.is_file()
        && let Some(table) = read_table(path, &Source::UserFile(path.clone()), &mut errors)
    {
        state.apply(&table, &Source::UserFile(path.clone()), &mut errors);
    }

    // 3. project file (walk up to the repo root; protected sections rejected)
    if let Some(dir) = &project_dir
        && let Some(path) = find_project_file(dir)
    {
        let source = Source::ProjectFile(path.clone());
        if let Some(mut table) = read_table(&path, &source, &mut errors) {
            strip_protected(&mut table, &source, &mut errors);
            state.apply(&table, &source, &mut errors);
        }
    }

    // 3b. --config / $KEVIN_CONFIG (must exist)
    let extra_file = config_file.or_else(|| {
        paths::env_value(&env, KEVIN_CONFIG_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });
    if let Some(path) = extra_file {
        let source = Source::ConfigFile(path.clone());
        if let Some(table) = read_table(&path, &source, &mut errors) {
            state.apply(&table, &source, &mut errors);
        }
    }

    // 4. environment: Kohral aliases first (lower), then KEVIN__* sorted by name.
    for (var, key) in KOHRAL_ENV_ALIASES {
        if let Some(value) = paths::env_value(&env, var).filter(|v| !v.is_empty()) {
            let source = Source::Env((*var).to_owned());
            let coerced = state.coerce(key, value);
            state.apply(&dotted(key, coerced), &source, &mut errors);
        }
    }
    let mut env_overrides: Vec<(String, String)> = env
        .iter()
        .filter(|(k, _)| k.starts_with(ENV_PREFIX) && k.len() > ENV_PREFIX.len())
        .cloned()
        .collect();
    env_overrides.sort();
    for (var, value) in env_overrides {
        let key = env_var_to_key(&var);
        let source = Source::Env(var);
        let coerced = state.coerce(&key, &value);
        state.apply(&dotted(&key, coerced), &source, &mut errors);
    }

    // 5. --set key=value
    for arg in &sets {
        match parse_set(arg) {
            Ok((key, raw)) => {
                let coerced = state.coerce(&key, &raw);
                state.apply(&dotted(&key, coerced), &Source::Set, &mut errors);
            }
            Err(message) => errors.push(ConfigError::InvalidSet {
                arg: arg.clone(),
                message,
            }),
        }
    }

    // Profile defaults (only where the key is still at its built-in default).
    let profile = state.typed().map_or(Profile::Laptop, |c| c.kevin.profile);
    for (key, value) in profile_overrides(profile) {
        if state.sources.get(key) == Some(&Source::Default) {
            state.merge(&dotted(key, value), &Source::Profile(profile));
        }
    }

    // Derived: sandbox.allow_dangerous_flags follows the tier unless set explicitly.
    if state.sources.get(ALLOW_DANGEROUS_KEY) == Some(&Source::Default) {
        let container = state
            .typed()
            .is_ok_and(|c| c.sandbox.tier == SandboxTier::Container);
        state.merge(
            &dotted(ALLOW_DANGEROUS_KEY, Value::Boolean(container)),
            &Source::Derived,
        );
    }

    let config = match state.typed() {
        Ok(config) => config,
        Err((key, message)) => {
            // Only reachable if the defaults themselves are broken.
            errors.push(ConfigError::Invalid {
                key,
                layer: Source::Default,
                message,
            });
            return Err(errors);
        }
    };
    validate::validate(&config, &state.sources, &mut errors);
    errors.into_result()?;
    Ok(Resolved {
        config,
        sources: state.sources,
    })
}

/// Cumulative table + per-leaf sources.
#[derive(Debug, Clone)]
struct Layered {
    table: Table,
    sources: Sources,
}

impl Layered {
    fn defaults() -> Self {
        let table = Table::try_from(KevinConfig::default())
            .unwrap_or_else(|e| unreachable!("defaults serialize to TOML: {e}"));
        let mut sources = BTreeMap::new();
        for path in leaf_paths(&table, "") {
            sources.insert(path, Source::Default);
        }
        Self { table, sources }
    }

    /// Deep-merges `layer` into the cumulative table, recording `source` for
    /// every leaf it sets.
    fn merge(&mut self, layer: &Table, source: &Source) {
        merge_into(&mut self.table, layer, "", source, &mut self.sources);
    }

    /// Merges `layer`, attributing schema failures to `source`. When the layer
    /// as a whole is rejected it is retried one unit at a time (a leaf key, or
    /// a whole `[models.<alias>]` table) so every offending key is reported and
    /// the valid keys of a partially-bad layer still apply.
    fn apply(&mut self, layer: &Table, source: &Source, errors: &mut ConfigErrors) {
        if self.try_merge(layer, source).is_ok() {
            return;
        }
        let units = units(layer);
        if units.len() <= 1 {
            if let Err((key, message)) = self.try_merge(layer, source) {
                errors.push(ConfigError::Invalid {
                    key,
                    layer: source.clone(),
                    message,
                });
            }
            return;
        }
        for (path, value) in units {
            if let Err((key, message)) = self.try_merge(&dotted(&path, value), source) {
                errors.push(ConfigError::Invalid {
                    key,
                    layer: source.clone(),
                    message,
                });
            }
        }
    }

    /// Merges `layer`; on a schema failure reverts and returns `(key, message)`.
    fn try_merge(&mut self, layer: &Table, source: &Source) -> Result<(), (String, String)> {
        let snapshot = self.clone();
        self.merge(layer, source);
        match self.typed() {
            Ok(_) => Ok(()),
            Err(e) => {
                *self = snapshot;
                Err(e)
            }
        }
    }

    /// Deserializes the cumulative table; `Err((key path, message))`.
    fn typed(&self) -> Result<KevinConfig, (String, String)> {
        serde_path_to_error::deserialize(self.table.clone()).map_err(|e| {
            let key = e.path().to_string();
            let key = if key == "." { String::new() } else { key };
            (key, e.inner().message().to_owned())
        })
    }

    /// Coerces a raw env/`--set` string to the type currently at `key`: strings
    /// stay strings, anything else is parsed as a TOML value when possible.
    fn coerce(&self, key: &str, raw: &str) -> Value {
        let existing = lookup(&self.table, key);
        match existing {
            Some(Value::String(_)) => Value::String(raw.to_owned()),
            _ => raw
                .parse::<Value>()
                .unwrap_or_else(|_| Value::String(raw.to_owned())),
        }
    }
}

fn merge_into(
    target: &mut Table,
    layer: &Table,
    prefix: &str,
    source: &Source,
    sources: &mut Sources,
) {
    for (key, value) in layer {
        let path = join(prefix, key);
        if let (Some(Value::Table(existing)), Value::Table(incoming)) = (target.get_mut(key), value)
        {
            merge_into(existing, incoming, &path, source, sources);
        } else {
            // Replace: drop stale sources under this path, record new leaves.
            let nested_prefix = format!("{path}.");
            sources.retain(|k, _| k != &path && !k.starts_with(&nested_prefix));
            match value {
                Value::Table(t) if !t.is_empty() => {
                    for leaf in leaf_paths(t, &path) {
                        sources.insert(leaf, source.clone());
                    }
                }
                _ => {
                    sources.insert(path.clone(), source.clone());
                }
            }
            target.insert(key.clone(), value.clone());
        }
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Splits a layer into independently applicable units: every leaf, except
/// that each `[models.<alias>]` table stays whole (its keys only make sense
/// together).
fn units(layer: &Table) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for (key, value) in layer {
        match value {
            Value::Table(aliases) if key == "models" => {
                for (alias, entry) in aliases {
                    out.push((format!("models.{alias}"), entry.clone()));
                }
            }
            Value::Table(t) if !t.is_empty() => {
                for path in leaf_paths(t, key) {
                    if let Some(v) = lookup(layer, &path) {
                        out.push((path, v.clone()));
                    }
                }
            }
            _ => out.push((key.clone(), value.clone())),
        }
    }
    out
}

/// Dotted paths of every non-table value (empty tables count as leaves).
fn leaf_paths(table: &Table, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (key, value) in table {
        let path = join(prefix, key);
        match value {
            Value::Table(t) if !t.is_empty() => out.extend(leaf_paths(t, &path)),
            _ => out.push(path),
        }
    }
    out
}

fn lookup<'a>(table: &'a Table, key: &str) -> Option<&'a Value> {
    let mut segments = key.split('.');
    let mut current = table.get(segments.next()?)?;
    for segment in segments {
        current = current.as_table()?.get(segment)?;
    }
    Some(current)
}

/// Builds `{ a: { b: value } }` from `"a.b"`.
fn dotted(key: &str, value: Value) -> Table {
    let mut segments: Vec<&str> = key.split('.').collect();
    let last = segments.pop().unwrap_or_default();
    let mut table = Table::new();
    table.insert(last.to_owned(), value);
    for segment in segments.into_iter().rev() {
        let mut outer = Table::new();
        outer.insert(segment.to_owned(), Value::Table(table));
        table = outer;
    }
    table
}

/// `KEVIN__SERVER__BIND` → `server.bind`.
fn env_var_to_key(var: &str) -> String {
    var.strip_prefix(ENV_PREFIX)
        .unwrap_or(var)
        .split("__")
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(".")
}

/// Parses `section.key=value`.
fn parse_set(arg: &str) -> Result<(String, String), String> {
    let Some((key, value)) = arg.split_once('=') else {
        return Err("expected `section.key=value`".into());
    };
    let key = key.trim();
    if key.is_empty() || key.split('.').any(str::is_empty) {
        return Err(format!("invalid key {key:?}: expected `section.key`"));
    }
    if !key.contains('.') {
        return Err(format!(
            "invalid key {key:?}: expected `section.key` (a dotted path)"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn read_table(path: &Path, source: &Source, errors: &mut ConfigErrors) -> Option<Table> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            errors.push(ConfigError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            });
            return None;
        }
    };
    match text.parse::<Table>() {
        Ok(table) => Some(table),
        Err(e) => {
            errors.push(ConfigError::Parse {
                layer: source.clone(),
                message: e.message().to_owned(),
            });
            None
        }
    }
}

/// Removes protected sections from a project-layer table, one error per leaf.
fn strip_protected(table: &mut Table, source: &Source, errors: &mut ConfigErrors) {
    for section in PROJECT_PROTECTED_SECTIONS {
        if let Some(value) = table.remove(*section) {
            let leaves = match &value {
                Value::Table(t) if !t.is_empty() => leaf_paths(t, section),
                _ => vec![(*section).to_owned()],
            };
            for key in leaves {
                errors.push(ConfigError::ProjectLayerNotAllowed {
                    key,
                    layer: source.clone(),
                });
            }
        }
    }
}

/// Walks up from `start` looking for `.kevin/kevin.toml`; stops after the
/// first directory that is a git/jj repo root.
#[must_use]
pub fn find_project_file(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    while let Some(current) = dir {
        let candidate = current.join(PROJECT_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if current.join(".git").exists() || current.join(".jj").exists() {
            return None;
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    None
}

/// The defaults a profile flips (`plan/03` §Validation rules), as
/// `(key, value)` pairs; applied only where the key is still at its default.
#[must_use]
pub fn profile_overrides(profile: Profile) -> Vec<(&'static str, Value)> {
    let server = || {
        vec![
            ("database.auto_migrate", Value::Boolean(false)),
            ("telemetry.log_format", Value::String("json".into())),
            ("server.docs", Value::Boolean(false)),
        ]
    };
    match profile {
        Profile::Laptop => Vec::new(),
        Profile::Server => server(),
        Profile::Kohral => {
            let mut v = server();
            v.extend([
                ("kohral.enabled", Value::Boolean(true)),
                ("kevin.auto_approve_plans", Value::Boolean(true)),
                ("server.bind", Value::String("0.0.0.0:7777".into())),
                (
                    "telemetry.metrics_bind",
                    Value::String("0.0.0.0:9464".into()),
                ),
            ]);
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_mapping() {
        assert_eq!(env_var_to_key("KEVIN__DATABASE__URL"), "database.url");
        assert_eq!(env_var_to_key("KEVIN__KEVIN__PROFILE"), "kevin.profile");
        assert_eq!(
            env_var_to_key("KEVIN__WORKERS__CLAUDE__MAX_TURNS"),
            "workers.claude.max_turns"
        );
    }

    #[test]
    fn set_parsing() {
        assert_eq!(
            parse_set("kevin.profile=kohral").unwrap(),
            ("kevin.profile".into(), "kohral".into())
        );
        assert_eq!(
            parse_set("database.url=postgres://a:b=c@h/d").unwrap().1,
            "postgres://a:b=c@h/d"
        );
        assert!(parse_set("profile=kohral").is_err());
        assert!(parse_set("kevin.profile").is_err());
        assert!(parse_set("=x").is_err());
        assert!(parse_set("a..b=x").is_err());
    }

    #[test]
    fn dotted_builds_nested_tables() {
        let t = dotted("a.b.c", Value::Integer(1));
        assert_eq!(t["a"]["b"]["c"], Value::Integer(1));
    }

    #[test]
    fn coercion_respects_existing_type() {
        let state = Layered::defaults();
        assert_eq!(
            state.coerce("server.bind", "0.0.0.0:1"),
            Value::String("0.0.0.0:1".into())
        );
        assert_eq!(state.coerce("budget.max_attempts", "3"), Value::Integer(3));
        assert_eq!(
            state.coerce("workers.claude.enabled", "false"),
            Value::Boolean(false)
        );
        assert_eq!(
            state.coerce("server.cors_origins", "[\"https://a\"]"),
            Value::Array(vec![Value::String("https://a".into())])
        );
        // Unknown path, unparsable → string.
        assert_eq!(
            state.coerce("models.x.model", "gpt-5.6"),
            Value::String("gpt-5.6".into())
        );
    }

    #[test]
    fn merge_tracks_sources_and_replaces_arrays() {
        let mut state = Layered::defaults();
        let layer: Table =
            "[server]\ncors_origins = [\"a\"]\n[concurrency.per_worker_kind]\nclaude = 1\n"
                .parse()
                .unwrap();
        state.merge(&layer, &Source::Set);
        assert_eq!(state.sources["server.cors_origins"], Source::Set);
        assert_eq!(
            state.sources["concurrency.per_worker_kind.claude"],
            Source::Set
        );
        assert_eq!(
            state.sources["concurrency.per_worker_kind.codex"],
            Source::Default
        );
        assert_eq!(state.sources["server.bind"], Source::Default);
    }

    #[test]
    fn project_file_discovery_stops_at_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(dir.path().join(".kevin")).unwrap();
        std::fs::write(dir.path().join(".kevin/kevin.toml"), "").unwrap();
        // Above the repo root: not found.
        assert_eq!(find_project_file(&nested), None);
        std::fs::create_dir_all(root.join(".kevin")).unwrap();
        std::fs::write(root.join(".kevin/kevin.toml"), "").unwrap();
        assert_eq!(
            find_project_file(&nested),
            Some(root.join(".kevin/kevin.toml"))
        );
    }
}
