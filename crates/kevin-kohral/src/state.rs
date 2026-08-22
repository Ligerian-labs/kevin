//! What every Kohral handler is given.
//!
//! The Kohral surface is deliberately *not* built on `kevin_api::AppState`:
//! it has its own token (`plan/07` §Authentication), its own error envelope
//! and a much smaller set of dependencies. It shares the two things worth
//! sharing — the constant-time [`TokenVerifier`] and the booted orchestrator
//! [`Handle`] — and nothing else.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kevin_api::auth::TokenVerifier;
use kevin_config::KevinConfig;
use kevin_domain::WorkerKind;
use kevin_orchestrator::Handle;
use kevin_router::ModelCatalog;

use crate::catalog::RuntimeCatalog;
use crate::ledger::RunsLedger;
use crate::projection::Narrative;
use crate::turn::TurnEnvironment;

/// Knobs of the Kohral listener that are not in `[kohral]`.
#[derive(Debug, Clone)]
pub struct KohralOptions {
    /// Reported as `version` in `/health` and `/v1/capabilities`.
    pub version: String,
    /// What `partial_output` carries.
    pub narrative: Narrative,
    /// Whether `PUT /v1/attachments/…` is served (and advertised).
    pub temporary_attachments: bool,
    /// Root of the ephemeral upload area; must stay `/tmp/kohral-uploads` in
    /// production because Kohral validates the returned path prefix.
    pub upload_root: PathBuf,
    /// `kohral.max_attachment_bytes`.
    pub max_attachment_bytes: u64,
    /// Where each run gets its working directory.
    pub work_root: PathBuf,
    /// `kohral.run_timeout`.
    pub run_timeout: Duration,
}

/// The production upload root; Kohral rejects any other prefix.
pub const UPLOAD_ROOT: &str = "/tmp/kohral-uploads";

impl KohralOptions {
    /// The options implied by a resolved configuration.
    #[must_use]
    pub fn from_config(config: &KevinConfig, version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            narrative: Narrative::for_config(config),
            temporary_attachments: true,
            upload_root: PathBuf::from(UPLOAD_ROOT),
            max_attachment_bytes: config.kohral.max_attachment_bytes,
            work_root: config.kevin.data_dir.join("work"),
            run_timeout: config.kohral.run_timeout,
        }
    }

    /// The per-turn environment the mapper needs.
    #[must_use]
    pub fn environment(&self) -> TurnEnvironment {
        TurnEnvironment {
            work_root: self.work_root.clone(),
            upload_root: self.upload_root.clone(),
            run_timeout: self.run_timeout,
        }
    }
}

/// Everything the handlers need; cheap to clone.
#[derive(Clone)]
pub struct KohralState {
    inner: Arc<Inner>,
}

struct Inner {
    handle: Arc<Handle>,
    ledger: RunsLedger,
    auth: Arc<TokenVerifier>,
    config: Arc<KevinConfig>,
    catalog: ModelCatalog,
    authenticated: BTreeSet<WorkerKind>,
    options: KohralOptions,
    started_at: Instant,
}

impl std::fmt::Debug for KohralState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KohralState")
            .field("version", &self.inner.options.version)
            .field("narrative", &self.inner.options.narrative)
            .field("authenticated", &self.inner.authenticated)
            .finish_non_exhaustive()
    }
}

impl KohralState {
    /// Wires the listener.
    ///
    /// `authenticated` is the set of worker kinds whose `doctor()` reported a
    /// usable binary **and** credentials; it is captured once at boot, because
    /// probing four CLIs on every `GET /v1/kohral/models` would make Kohral's
    /// model picker slow and flaky.
    #[must_use]
    pub fn new(
        handle: Arc<Handle>,
        ledger: RunsLedger,
        auth: Arc<TokenVerifier>,
        config: Arc<KevinConfig>,
        authenticated: BTreeSet<WorkerKind>,
        options: KohralOptions,
    ) -> Self {
        let catalog = ModelCatalog::from_config(&config);
        Self {
            inner: Arc::new(Inner {
                handle,
                ledger,
                auth,
                config,
                catalog,
                authenticated,
                options,
                started_at: Instant::now(),
            }),
        }
    }

    /// The orchestrator.
    #[must_use]
    pub fn handle(&self) -> &Arc<Handle> {
        &self.inner.handle
    }

    /// The durable turn ledger.
    #[must_use]
    pub fn ledger(&self) -> &RunsLedger {
        &self.inner.ledger
    }

    /// The Kohral bearer token verifier.
    #[must_use]
    pub fn auth(&self) -> &Arc<TokenVerifier> {
        &self.inner.auth
    }

    /// The effective configuration.
    #[must_use]
    pub fn config(&self) -> &Arc<KevinConfig> {
        &self.inner.config
    }

    /// The listener's options.
    #[must_use]
    pub fn options(&self) -> &KohralOptions {
        &self.inner.options
    }

    /// The model catalog Kohral's picker is fed from.
    #[must_use]
    pub fn runtime_catalog(&self) -> RuntimeCatalog {
        let authenticated = self.inner.authenticated.clone();
        RuntimeCatalog::build(&self.inner.catalog, &move |kind| {
            authenticated.contains(&kind)
        })
    }

    /// Whether admission is closed. The orchestrator's own gate is the single
    /// source of truth, so `/v1/maintenance/drain` and
    /// `/api/v1/maintenance/drain` can never disagree.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        !self.inner.handle.is_admitting()
    }

    /// Seconds since the listener was built.
    #[must_use]
    pub fn uptime_seconds(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }
}
