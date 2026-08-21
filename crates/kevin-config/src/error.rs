//! Configuration errors: one variant per failure class, aggregated in
//! [`ConfigErrors`] so `kevin config validate` reports everything at once.

use std::fmt;
use std::path::PathBuf;

use kevin_domain::{ModelAlias, WorkerKind};

use crate::source::Source;

/// One configuration problem, with the key path and the layer it came from
/// whenever those are known.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A config file could not be read.
    #[error("cannot read {path}: {message}")]
    Io {
        /// File path.
        path: PathBuf,
        /// OS error text.
        message: String,
    },
    /// A config file or `--set` value is not valid TOML.
    #[error("{layer}: invalid TOML: {message}")]
    Parse {
        /// Layer that failed to parse.
        layer: Source,
        /// Parser message.
        message: String,
    },
    /// A value does not fit the schema (unknown key, wrong type, bad enum or
    /// duration). `key` is the dotted path as far as it could be determined.
    #[error("{key} ({layer}): {message}")]
    Invalid {
        /// Dotted key path.
        key: String,
        /// Layer that introduced the value.
        layer: Source,
        /// Deserializer message.
        message: String,
    },
    /// `--set` argument not of the form `section.key=value`.
    #[error("--set {arg:?}: {message}")]
    InvalidSet {
        /// The raw argument.
        arg: String,
        /// What is wrong with it.
        message: String,
    },
    /// A project-layer file (`./.kevin/kevin.toml`) tried to set a protected key.
    #[error(
        "{key} ({layer}): project-layer config may not set sandbox.*, workers.*, server.*, database.* or kohral.*"
    )]
    ProjectLayerNotAllowed {
        /// Dotted key path.
        key: String,
        /// The project file.
        layer: Source,
    },
    /// `server.bind` is not loopback and no auth token file is configured.
    #[error(
        "server.bind ({layer}): binding {bind} (non-loopback) requires a non-empty server.auth_token_file (or kohral.token_file with the kohral profile)"
    )]
    InsecureBind {
        /// The offending bind address.
        bind: String,
        /// Layer that set the bind.
        layer: Source,
    },
    /// `database.url` is not a `postgres://` URL.
    #[error("database.url ({layer}): {message}")]
    InvalidDatabaseUrl {
        /// Layer that set the URL.
        layer: Source,
        /// Why it is invalid.
        message: String,
    },
    /// Both `database.url` and `database.url_file` were set (or neither).
    #[error("database ({layer}): exactly one of database.url and database.url_file must be set")]
    DatabaseUrlExactlyOne {
        /// Layer that set the conflicting/missing value.
        layer: Source,
    },
    /// A role or routing candidate names an alias that has no `[models.<alias>]` entry.
    #[error("{key} ({layer}): unknown model alias {alias:?} (no [models.{alias}] entry)")]
    UnknownModelAlias {
        /// Dotted key path (e.g. `roles.planner`, `routing.kinds.implement.candidates[1]`).
        key: String,
        /// Layer that set the reference.
        layer: Source,
        /// The unknown alias.
        alias: ModelAlias,
    },
    /// A role or routing candidate names an alias whose worker is disabled.
    #[error("{key} ({layer}): alias {alias:?} uses worker {worker:?} which is disabled")]
    ModelWorkerDisabled {
        /// Dotted key path.
        key: String,
        /// Layer that set the reference.
        layer: Source,
        /// The alias.
        alias: ModelAlias,
        /// Its (disabled) worker.
        worker: WorkerKind,
    },
    /// A `[models.<alias>]` entry is not valid for its worker (e.g. `pi` without `provider`).
    #[error("models.{alias} ({layer}): {message}")]
    InvalidModelEntry {
        /// The alias.
        alias: ModelAlias,
        /// Layer that defined the entry.
        layer: Source,
        /// Why it is invalid.
        message: String,
    },
    /// A dangerous worker flag/mode is set while `sandbox.tier != "container"`.
    #[error(
        "{key} ({layer}): {value:?} requires sandbox.tier = \"container\" (current tier: {tier})"
    )]
    ForbiddenOutsideContainer {
        /// Dotted key path.
        key: String,
        /// Layer that set it.
        layer: Source,
        /// The offending value.
        value: String,
        /// The configured tier.
        tier: String,
    },
    /// A numeric value is out of its documented range.
    #[error("{key} ({layer}): {message}")]
    OutOfRange {
        /// Dotted key path.
        key: String,
        /// Layer that set it.
        layer: Source,
        /// Constraint text.
        message: String,
    },
    /// `memory.dimensions` does not match the embedding model's known dimension.
    #[error(
        "memory.dimensions ({layer}): {actual} does not match embedding model {model:?} ({expected} dimensions)"
    )]
    EmbeddingDimensionMismatch {
        /// Layer that set `memory.dimensions`.
        layer: Source,
        /// The model.
        model: String,
        /// Known dimension of the model.
        expected: u32,
        /// Configured dimension.
        actual: u32,
    },
}

impl ConfigError {
    /// Error for a `[models.<alias>]` entry rejected by a worker adapter
    /// (`Worker::validate_alias`); the source is filled by the loader when known.
    #[must_use]
    pub fn invalid_model_entry(alias: ModelAlias, message: impl Into<String>) -> Self {
        ConfigError::InvalidModelEntry {
            alias,
            layer: Source::Unknown,
            message: message.into(),
        }
    }

    /// The dotted key path this error refers to, when it has one.
    #[must_use]
    pub fn key(&self) -> Option<String> {
        match self {
            ConfigError::Invalid { key, .. }
            | ConfigError::ProjectLayerNotAllowed { key, .. }
            | ConfigError::UnknownModelAlias { key, .. }
            | ConfigError::ModelWorkerDisabled { key, .. }
            | ConfigError::ForbiddenOutsideContainer { key, .. }
            | ConfigError::OutOfRange { key, .. } => Some(key.clone()),
            ConfigError::InsecureBind { .. } => Some("server.bind".into()),
            ConfigError::InvalidDatabaseUrl { .. } => Some("database.url".into()),
            ConfigError::DatabaseUrlExactlyOne { .. } => Some("database".into()),
            ConfigError::InvalidModelEntry { alias, .. } => Some(format!("models.{alias}")),
            ConfigError::EmbeddingDimensionMismatch { .. } => Some("memory.dimensions".into()),
            ConfigError::Io { .. } | ConfigError::Parse { .. } | ConfigError::InvalidSet { .. } => {
                None
            }
        }
    }
}

/// Every problem found while loading/validating, reported together.
#[derive(Debug, Clone, PartialEq, Eq, Default, thiserror::Error)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl ConfigErrors {
    /// `true` when there is at least one error.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of errors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Iterates over the errors.
    pub fn iter(&self) -> impl Iterator<Item = &ConfigError> {
        self.0.iter()
    }

    /// Adds an error.
    pub fn push(&mut self, err: ConfigError) {
        self.0.push(err);
    }

    /// `Ok(())` when empty, `Err(self)` otherwise.
    pub fn into_result(self) -> Result<(), ConfigErrors> {
        if self.0.is_empty() { Ok(()) } else { Err(self) }
    }
}

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.len() {
            0 => write!(f, "configuration is valid"),
            1 => write!(f, "1 configuration error:"),
            n => write!(f, "{n} configuration errors:"),
        }?;
        for err in &self.0 {
            write!(f, "\n  - {err}")?;
        }
        Ok(())
    }
}

impl From<ConfigError> for ConfigErrors {
    fn from(err: ConfigError) -> Self {
        ConfigErrors(vec![err])
    }
}

impl IntoIterator for ConfigErrors {
    type Item = ConfigError;
    type IntoIter = std::vec::IntoIter<ConfigError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
