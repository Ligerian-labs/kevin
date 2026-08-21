//! [`WorkerRegistry`] — the enabled workers built from configuration
//! (`plan/04-workers.md` §Registry and doctor).
//!
//! The registry takes a [`RegistryConfig`] — the slice of `KevinConfig` the
//! workers need (`[workers]`, `[workers.*]`, `[models]`,
//! `[sandbox].env_allowlist_extra`, `[concurrency].per_worker_kind`,
//! `kevin.data_dir`), obtained with `RegistryConfig::from(&KevinConfig)`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kevin_config::{KevinConfig, Source};
use kevin_domain::{ModelAlias, WorkerKind};

use crate::fake::{FakeWorker, Scenario};
use crate::policy::SandboxPolicy;
use crate::types::{ConfigError, ConfigErrors, EnvAllowlist, ModelEntry};
use crate::worker::{AuthStatus, Doctor, Worker};

/// Timeout for `<bin> --version` probes.
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Common subset of `[workers.<kind>]` every adapter needs (adapters read
/// their kind-specific keys from `kevin_config::Workers` directly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCfg {
    /// `workers.<kind>.enabled`.
    pub enabled: bool,
    /// `workers.<kind>.bin` (empty for the fake).
    pub bin: String,
    /// `workers.<kind>.extra_args`.
    pub extra_args: Vec<String>,
    /// `workers.<kind>.env_passthrough`.
    pub env_passthrough: Vec<String>,
}

impl WorkerCfg {
    /// An enabled CLI worker with the given binary and passthrough list.
    pub fn cli(bin: &str, env_passthrough: &[&str]) -> Self {
        Self {
            enabled: true,
            bin: bin.to_owned(),
            extra_args: Vec::new(),
            env_passthrough: env_passthrough.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// A disabled worker.
    #[must_use]
    pub fn disabled(bin: &str) -> Self {
        Self {
            enabled: false,
            bin: bin.to_owned(),
            extra_args: Vec::new(),
            env_passthrough: Vec::new(),
        }
    }
}

/// The configuration slice the registry needs (`plan/03-config-schema.md`).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistryConfig {
    /// `kevin.data_dir` (transcripts under `<data_dir>/runs/…`).
    pub data_dir: PathBuf,
    /// `workers.kill_grace`.
    pub kill_grace: Duration,
    /// `[workers.<kind>]` per kind (every kind present).
    pub workers: BTreeMap<WorkerKind, WorkerCfg>,
    /// `[workers.claude]` in full (the adapter needs its kind-specific keys).
    pub claude: kevin_config::ClaudeWorker,
    /// `[workers.codex]` in full (the adapter needs its kind-specific keys).
    pub codex: kevin_config::CodexWorker,
    /// `[workers.opencode]` in full (the adapter needs its kind-specific keys).
    pub opencode: kevin_config::OpencodeWorker,
    /// `workers.fake.script` (`None` when empty → built-in scenario).
    pub fake_script: Option<PathBuf>,
    /// `[models.*]`.
    pub models: BTreeMap<ModelAlias, ModelEntry>,
    /// `sandbox.env_allowlist_extra`.
    pub env_allowlist_extra: Vec<String>,
    /// `concurrency.per_worker_kind`.
    pub per_worker_kind: BTreeMap<WorkerKind, u16>,
}

impl Default for RegistryConfig {
    /// The `plan/03-config-schema.md` defaults (`KevinConfig::default()`).
    fn default() -> Self {
        Self::from(&KevinConfig::default())
    }
}

impl From<&KevinConfig> for RegistryConfig {
    fn from(cfg: &KevinConfig) -> Self {
        let w = &cfg.workers;
        let cli = |enabled: bool, bin: &str, extra_args: &[String], env: &[String]| WorkerCfg {
            enabled,
            bin: bin.to_owned(),
            extra_args: extra_args.to_vec(),
            env_passthrough: env.to_vec(),
        };
        Self {
            data_dir: cfg.kevin.data_dir.clone(),
            kill_grace: w.kill_grace,
            workers: BTreeMap::from([
                (
                    WorkerKind::Claude,
                    cli(
                        w.claude.enabled,
                        &w.claude.bin,
                        &w.claude.extra_args,
                        &w.claude.env_passthrough,
                    ),
                ),
                (
                    WorkerKind::Codex,
                    cli(
                        w.codex.enabled,
                        &w.codex.bin,
                        &w.codex.extra_args,
                        &w.codex.env_passthrough,
                    ),
                ),
                (
                    WorkerKind::Pi,
                    cli(
                        w.pi.enabled,
                        &w.pi.bin,
                        &w.pi.extra_args,
                        &w.pi.env_passthrough,
                    ),
                ),
                (
                    WorkerKind::Opencode,
                    cli(
                        w.opencode.enabled,
                        &w.opencode.bin,
                        &w.opencode.extra_args,
                        &w.opencode.env_passthrough,
                    ),
                ),
                (
                    WorkerKind::Fake,
                    WorkerCfg {
                        enabled: w.fake.enabled,
                        ..WorkerCfg::disabled("")
                    },
                ),
            ]),
            claude: w.claude.clone(),
            codex: w.codex.clone(),
            opencode: w.opencode.clone(),
            fake_script: (!w.fake.script.as_os_str().is_empty()).then(|| w.fake.script.clone()),
            models: cfg.models.clone(),
            env_allowlist_extra: cfg.sandbox.env_allowlist_extra.clone(),
            per_worker_kind: cfg
                .concurrency
                .per_worker_kind
                .iter()
                .map(|(k, n)| (*k, u16::try_from(*n).unwrap_or(u16::MAX)))
                .collect(),
        }
    }
}

impl RegistryConfig {
    /// Test configuration: only the fake worker (built-in scenario) and the
    /// `fake` alias, transcripts under `data_dir`.
    pub fn fake_only(data_dir: impl Into<PathBuf>) -> Self {
        let mut cfg = Self {
            data_dir: data_dir.into(),
            ..Self::default()
        };
        for (kind, worker) in &mut cfg.workers {
            worker.enabled = *kind == WorkerKind::Fake;
        }
        cfg.models
            .retain(|_, entry| entry.worker == WorkerKind::Fake);
        cfg
    }

    /// Sets `workers.fake.script`.
    #[must_use]
    pub fn with_fake_script(mut self, path: impl Into<PathBuf>) -> Self {
        self.fake_script = Some(path.into());
        self
    }

    /// Enables/disables one worker.
    #[must_use]
    pub fn enable(mut self, kind: WorkerKind, enabled: bool) -> Self {
        self.workers
            .entry(kind)
            .or_insert_with(|| WorkerCfg::disabled(""))
            .enabled = enabled;
        self
    }

    /// Sets `workers.<kind>.bin`.
    #[must_use]
    pub fn with_bin(mut self, kind: WorkerKind, bin: impl Into<String>) -> Self {
        self.workers
            .entry(kind)
            .or_insert_with(|| WorkerCfg::disabled(""))
            .bin = bin.into();
        self
    }

    /// The `[workers.<kind>]` entry (a disabled default when absent).
    #[must_use]
    pub fn worker(&self, kind: WorkerKind) -> WorkerCfg {
        self.workers
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| WorkerCfg::disabled(kind.as_str()))
    }

    /// Enabled kinds in [`WorkerKind::ALL`] order.
    pub fn enabled_kinds(&self) -> impl Iterator<Item = WorkerKind> + '_ {
        WorkerKind::ALL
            .into_iter()
            .filter(|k| self.workers.get(k).is_some_and(|w| w.enabled))
    }

    /// `workers.<kind>.env_passthrough` ∪ `sandbox.env_allowlist_extra`.
    #[must_use]
    pub fn env_allowlist(&self, kind: WorkerKind) -> EnvAllowlist {
        EnvAllowlist::build(
            &self.worker(kind).env_passthrough,
            &self.env_allowlist_extra,
        )
    }

    /// Aliases served by `kind`, sorted.
    #[must_use]
    pub fn aliases_for(&self, kind: WorkerKind) -> Vec<ModelAlias> {
        self.models
            .iter()
            .filter(|(_, e)| e.worker == kind)
            .map(|(a, _)| a.clone())
            .collect()
    }
}

/// Builds an adapter for one kind from configuration.
pub type WorkerFactory = Arc<
    dyn Fn(&RegistryConfig, &WorkerCfg, &SandboxPolicy) -> Result<Arc<dyn Worker>, ConfigError>
        + Send
        + Sync,
>;

/// Adapters available in this build. WS-06/13/14/15 add their kinds here.
fn builtin_factory(kind: WorkerKind) -> Option<WorkerFactory> {
    match kind {
        WorkerKind::Fake => Some(Arc::new(|cfg, _worker, _policy| {
            let scenario = match &cfg.fake_script {
                Some(path) => Scenario::load(path).map_err(|e| ConfigError::Invalid {
                    key: "workers.fake.script".to_owned(),
                    layer: Source::Default,
                    message: e.to_string(),
                })?,
                None => Scenario::builtin(),
            };
            Ok(Arc::new(FakeWorker::new(scenario, cfg.data_dir.clone())) as Arc<dyn Worker>)
        })),
        WorkerKind::Claude => Some(Arc::new(|cfg, worker, policy| {
            let mut claude = crate::claude::ClaudeWorker::from_registry_config(cfg, policy)?;
            claude.set_bin(&worker.bin);
            Ok(Arc::new(claude) as Arc<dyn Worker>)
        })),
        WorkerKind::Codex => Some(Arc::new(|cfg, worker, policy| {
            let mut codex = crate::codex::CodexWorker::from_registry_config(cfg, policy)?;
            codex.set_bin(&worker.bin);
            Ok(Arc::new(codex) as Arc<dyn Worker>)
        })),
        WorkerKind::Opencode => Some(Arc::new(|cfg, worker, policy| {
            let mut opencode = crate::opencode::OpencodeWorker::from_registry_config(cfg, policy)?;
            opencode.set_bin(&worker.bin);
            Ok(Arc::new(opencode) as Arc<dyn Worker>)
        })),
        // TODO(ws-14): pi.
        WorkerKind::Pi => None,
    }
}

/// The enabled workers, keyed by kind.
#[derive(Clone)]
pub struct WorkerRegistry {
    map: HashMap<WorkerKind, Arc<dyn Worker>>,
    config: RegistryConfig,
    policy: SandboxPolicy,
}

impl std::fmt::Debug for WorkerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRegistry")
            .field("kinds", &self.kinds())
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl WorkerRegistry {
    /// An empty registry with `policy` (tests and adapters use [`WorkerRegistry::insert`]).
    #[must_use]
    pub fn empty(config: RegistryConfig, policy: SandboxPolicy) -> Self {
        Self {
            map: HashMap::new(),
            config,
            policy,
        }
    }

    /// Builds every enabled worker that has an adapter in this build and runs
    /// `validate_alias` for every `[models.*]` served by a registered worker.
    /// Errors are aggregated.
    pub fn from_config(cfg: &RegistryConfig, sandbox: SandboxPolicy) -> Result<Self, ConfigErrors> {
        let mut registry = Self::empty(cfg.clone(), sandbox);
        let mut errors = Vec::new();
        for kind in cfg.enabled_kinds() {
            let worker_cfg = cfg.worker(kind);
            if let Some(factory) = builtin_factory(kind) {
                match factory(cfg, &worker_cfg, &sandbox) {
                    Ok(worker) => {
                        registry.map.insert(kind, worker);
                    }
                    Err(err) => errors.push(err),
                }
            } else {
                tracing::debug!(kind = %kind, "worker enabled but no adapter in this build");
            }
        }
        for (alias, entry) in &cfg.models {
            if let Some(worker) = registry.map.get(&entry.worker)
                && let Err(err) = worker.validate_alias(alias, entry)
            {
                errors.push(err);
            }
        }
        if errors.is_empty() {
            Ok(registry)
        } else {
            Err(ConfigErrors(errors))
        }
    }

    /// Registers (or replaces) a worker.
    pub fn insert(&mut self, worker: Arc<dyn Worker>) {
        self.map.insert(worker.kind(), worker);
    }

    /// The worker for `kind`, if registered.
    #[must_use]
    pub fn get(&self, kind: WorkerKind) -> Option<Arc<dyn Worker>> {
        self.map.get(&kind).cloned()
    }

    /// Registered kinds in [`WorkerKind::ALL`] order.
    #[must_use]
    pub fn kinds(&self) -> Vec<WorkerKind> {
        WorkerKind::ALL
            .into_iter()
            .filter(|k| self.map.contains_key(k))
            .collect()
    }

    /// The sandbox policy adapters must consult.
    #[must_use]
    pub const fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// The configuration the registry was built from.
    #[must_use]
    pub const fn config(&self) -> &RegistryConfig {
        &self.config
    }

    /// One [`Doctor`] per *enabled* worker (config order). Enabled kinds
    /// without an adapter in this build are probed generically (binary +
    /// version, auth unknown).
    pub async fn doctor_all(&self) -> Vec<Doctor> {
        let mut out = Vec::new();
        for kind in self.config.enabled_kinds() {
            let doctor = if let Some(worker) = self.map.get(&kind) {
                worker.doctor().await
            } else {
                let mut d = probe_binary(kind, &self.config.worker(kind).bin).await;
                d.notes
                    .push("adapter not available in this build".to_owned());
                d
            };
            out.push(doctor);
        }
        out
    }
}

/// Finds `bin` on `PATH` (or verifies a path containing `/`).
#[must_use]
pub fn locate_binary(bin: &str) -> Option<PathBuf> {
    if bin.is_empty() {
        return None;
    }
    let candidate = Path::new(bin);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return is_executable(candidate).then(|| candidate.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Generic doctor probe: locate `bin`, run `<bin> --version` (bounded by
/// [`VERSION_PROBE_TIMEOUT`]), auth unknown. Never panics; never invokes
/// anything when the binary is missing.
pub async fn probe_binary(kind: WorkerKind, bin: &str) -> Doctor {
    let Some(path) = locate_binary(bin) else {
        return Doctor::missing(kind, bin);
    };
    let mut doctor = Doctor {
        kind,
        binary: Some(path.clone()),
        version: None,
        auth_ready: AuthStatus::Unknown,
        notes: Vec::new(),
    };
    let mut cmd = tokio::process::Command::new(&path);
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(VERSION_PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => {
            let text = if output.stdout.is_empty() {
                output.stderr
            } else {
                output.stdout
            };
            let first = String::from_utf8_lossy(&text)
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned);
            if output.status.success() {
                doctor.version = first;
            } else {
                doctor.notes.push(format!(
                    "`{} --version` exited {}{}",
                    path.display(),
                    output.status.code().unwrap_or(-1),
                    first.map(|f| format!(": {f}")).unwrap_or_default()
                ));
            }
        }
        Ok(Err(err)) => doctor
            .notes
            .push(format!("cannot run `{} --version`: {err}", path.display())),
        Err(_) => doctor.notes.push(format!(
            "`{} --version` timed out after {VERSION_PROBE_TIMEOUT:?}",
            path.display()
        )),
    }
    doctor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_mirror_plan_03() {
        let cfg = RegistryConfig::default();
        assert_eq!(cfg.kill_grace, Duration::from_secs(10));
        assert_eq!(
            cfg.enabled_kinds().collect::<Vec<_>>(),
            vec![
                WorkerKind::Claude,
                WorkerKind::Codex,
                WorkerKind::Pi,
                WorkerKind::Opencode
            ]
        );
        assert_eq!(
            cfg.worker(WorkerKind::Codex).extra_args,
            vec!["--skip-git-repo-check"]
        );
        assert_eq!(cfg.per_worker_kind[&WorkerKind::Fake], 64);
        assert_eq!(cfg.aliases_for(WorkerKind::Claude).len(), 4);
        assert!(
            cfg.env_allowlist(WorkerKind::Claude)
                .contains("ANTHROPIC_API_KEY")
        );
        let fake_only = RegistryConfig::fake_only("/tmp/x");
        assert_eq!(
            fake_only.enabled_kinds().collect::<Vec<_>>(),
            vec![WorkerKind::Fake]
        );
        assert_eq!(fake_only.models.len(), 1);
    }

    #[tokio::test]
    async fn registry_from_fake_only_config_registers_fake_and_validates_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = RegistryConfig::fake_only(dir.path());
        let registry = WorkerRegistry::from_config(&cfg, SandboxPolicy::cli_native()).unwrap();
        assert_eq!(registry.kinds(), vec![WorkerKind::Fake]);
        assert!(registry.get(WorkerKind::Fake).is_some());
        assert!(registry.get(WorkerKind::Claude).is_none());
        let doctors = registry.doctor_all().await;
        assert_eq!(doctors.len(), 1);
        assert!(doctors[0].is_healthy());

        let bad =
            RegistryConfig::fake_only(dir.path()).with_fake_script(dir.path().join("nope.yaml"));
        let err = WorkerRegistry::from_config(&bad, SandboxPolicy::cli_native()).unwrap_err();
        assert!(
            matches!(&err.0[0], ConfigError::Invalid { key, .. } if key == "workers.fake.script"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn probe_reports_missing_binary_without_running_anything() {
        let d = probe_binary(WorkerKind::Claude, "definitely-not-a-binary-kevin").await;
        assert!(d.binary.is_none());
        assert!(!d.is_healthy());
        assert!(locate_binary("").is_none());
        assert!(locate_binary("/definitely/not/here").is_none());
    }
}
