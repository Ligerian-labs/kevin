//! [`LocalWorkspace`] — the production [`WorkspacePort`] over `kevin-workspace`.
//!
//! `WorkspaceManager` and `Integrator` shell out to `git` / `jj` / `gh` and are
//! synchronous by design, so every call is moved onto a blocking thread. This
//! is the adapter WS-12 wires into [`crate::Deps`]; tests use
//! [`crate::testing::TempWorkspaces`] instead.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use kevin_config::{KevinConfig, WorkspaceCleanup};
use kevin_domain::Workspace;
use kevin_workspace::{
    IntegrationConfig, IntegrationRun, Integrator, PrepareRequest, WorkspaceManager,
};

use crate::convert;
use crate::ports::{
    IntegrateRequest, IntegrationOutcome, PortError, PortResult, PrepareWorkspace, WorkspacePort,
};

/// Per-attempt git worktrees / jj workspaces and result integration.
#[derive(Debug, Clone)]
pub struct LocalWorkspace {
    manager: Arc<WorkspaceManager>,
    integrator: Arc<Integrator>,
    cleanup: WorkspaceCleanup,
    integration: kevin_config::Integration,
}

impl LocalWorkspace {
    /// Builds the adapter for the repository at `repo_root`.
    pub fn new(repo_root: impl AsRef<Path>, config: &KevinConfig) -> PortResult<Self> {
        let repo_root = repo_root.as_ref();
        let manager = WorkspaceManager::new(repo_root, config.workspace.clone())
            .map_err(|e| PortError::permanent("workspace", e.to_string()))?;
        let integrator = Integrator::new(repo_root, IntegrationConfig::from_config(config))
            .map_err(|e| PortError::permanent("workspace", e.to_string()))?;
        Ok(Self {
            manager: Arc::new(manager),
            integrator: Arc::new(integrator),
            cleanup: config.workspace.cleanup,
            integration: config.workspace.integration,
        })
    }

    /// The repository root the adapter works in.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        self.manager.repo_root()
    }
}

#[async_trait]
impl WorkspacePort for LocalWorkspace {
    async fn prepare(&self, req: PrepareWorkspace) -> PortResult<Workspace> {
        let manager = Arc::clone(&self.manager);
        let request = PrepareRequest::new(req.run_id, req.task_id, req.attempt_id)
            .with_slug(&req.task_slug)
            .with_policy(convert::policy_to_manager(req.policy));
        blocking(move || manager.prepare_with(&request))
            .await?
            .map(|workspace| convert::workspace_from_manager(&workspace))
            .map_err(|e| PortError::transient("workspace", e.to_string()))
    }

    async fn cleanup(&self, workspace: &Workspace, succeeded: bool) -> PortResult<()> {
        let manager = Arc::clone(&self.manager);
        let workspace = convert::workspace_to_manager(workspace);
        let policy = self.cleanup;
        blocking(move || manager.cleanup(&workspace, policy, succeeded))
            .await?
            .map(|_| ())
            .map_err(|e| PortError::transient("workspace", e.to_string()))
    }

    async fn integrate(&self, req: IntegrateRequest) -> PortResult<IntegrationOutcome> {
        let integrator = Arc::clone(&self.integrator);
        let mode = self.integration;
        let run = IntegrationRun {
            run_id: req.run_id,
            title: req.title,
            summary: req.summary,
            acceptance_criteria: req.acceptance_criteria,
            base_branch: None,
        };
        let workspaces: Vec<kevin_workspace::Workspace> = req
            .workspaces
            .iter()
            .map(convert::workspace_to_manager)
            .collect();
        let result = blocking(move || integrator.integrate(&run, &workspaces, mode))
            .await?
            .map_err(|e| PortError::transient("workspace", e.to_string()))?;
        Ok(IntegrationOutcome {
            artifacts: result
                .artifacts
                .iter()
                .map(convert::artifact_from_manager)
                .collect(),
            conflicts: result
                .conflicts
                .into_iter()
                .map(|conflict| {
                    if conflict.files.is_empty() {
                        conflict.source
                    } else {
                        format!("{} ({})", conflict.source, conflict.files.join(", "))
                    }
                })
                .collect(),
        })
    }
}

async fn blocking<T, F>(f: F) -> PortResult<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| PortError::transient("workspace", format!("blocking task failed: {e}")))
}
