//! Where a configuration value came from, and the inputs of [`crate::load`].

use std::fmt;
use std::path::PathBuf;

use crate::schema::Profile;

/// Environment variable naming an extra config file (same as `--config`).
pub const KEVIN_CONFIG_ENV: &str = "KEVIN_CONFIG";
/// Prefix of environment overrides: `KEVIN__<SECTION>__<KEY>`.
pub const ENV_PREFIX: &str = "KEVIN__";
/// Kohral aliases of `kohral.*` keys read from the environment.
pub const KOHRAL_ENV_ALIASES: &[(&str, &str)] = &[
    ("KOHRAL_COLLABORATION_URL", "kohral.collaboration_url"),
    ("KOHRAL_RUNTIME_TOKEN_FILE", "kohral.token_file"),
];

/// The layer a value was taken from (lowest → highest precedence, then the
/// synthetic sources).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Source {
    /// Built-in defaults (`KevinConfig::default()`).
    Default,
    /// Default flipped by `kevin.profile` (`server` / `kohral`).
    Profile(Profile),
    /// User file (`$XDG_CONFIG_HOME/kevin/kevin.toml`).
    UserFile(PathBuf),
    /// Project file (`./.kevin/kevin.toml`, walking up to the repo root).
    ProjectFile(PathBuf),
    /// `--config <file>` or `$KEVIN_CONFIG`.
    ConfigFile(PathBuf),
    /// Environment variable (`KEVIN__…` or a Kohral alias); carries the variable name.
    Env(String),
    /// `--set key=value`.
    Set,
    /// Computed from other keys (e.g. `sandbox.allow_dangerous_flags`).
    Derived,
    /// Not tracked (errors raised outside the loader, e.g. by a worker adapter).
    Unknown,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Default => f.write_str("default"),
            Source::Profile(p) => write!(f, "profile:{p}"),
            Source::UserFile(p) => write!(f, "user:{}", p.display()),
            Source::ProjectFile(p) => write!(f, "project:{}", p.display()),
            Source::ConfigFile(p) => write!(f, "file:{}", p.display()),
            Source::Env(name) => write!(f, "env:{name}"),
            Source::Set => f.write_str("--set"),
            Source::Derived => f.write_str("derived"),
            Source::Unknown => f.write_str("unknown"),
        }
    }
}

/// Inputs of [`crate::load`]. `Default` is fully hermetic: no files, no
/// environment, no overrides — tests start there; the CLI uses
/// [`LoadOptions::from_process`].
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// User config file; `None` = no user layer. Missing file = layer skipped.
    pub user_file: Option<PathBuf>,
    /// Directory to walk up from looking for `.kevin/kevin.toml`; `None` = no project layer.
    pub project_dir: Option<PathBuf>,
    /// `--config <file>`; must exist when set.
    pub config_file: Option<PathBuf>,
    /// Environment to read `KEVIN__*`, `KEVIN_CONFIG` and Kohral aliases from.
    pub env: Vec<(String, String)>,
    /// `--set section.key=value` overrides, highest precedence.
    pub sets: Vec<String>,
}

impl LoadOptions {
    /// Hermetic options: defaults only.
    #[must_use]
    pub fn hermetic() -> Self {
        Self::default()
    }

    /// The CLI's options: XDG user file, project file from the current
    /// directory, the real process environment, plus `--config` / `--set`.
    #[must_use]
    pub fn from_process(config_file: Option<PathBuf>, sets: Vec<String>) -> Self {
        let env: Vec<(String, String)> = std::env::vars().collect();
        Self {
            user_file: Some(crate::paths::user_config_file(&env)),
            project_dir: std::env::current_dir().ok(),
            config_file,
            env,
            sets,
        }
    }

    /// Builder: user file.
    #[must_use]
    pub fn user_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.user_file = Some(path.into());
        self
    }

    /// Builder: project directory.
    #[must_use]
    pub fn project_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.project_dir = Some(path.into());
        self
    }

    /// Builder: `--config` file.
    #[must_use]
    pub fn config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_file = Some(path.into());
        self
    }

    /// Builder: one environment variable.
    #[must_use]
    pub fn env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Builder: one `--set key=value`.
    #[must_use]
    pub fn set(mut self, kv: impl Into<String>) -> Self {
        self.sets.push(kv.into());
        self
    }
}
