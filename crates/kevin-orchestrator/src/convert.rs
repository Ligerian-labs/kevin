//! Conversions between the domain value objects and the copies
//! `kevin-worker` / `kevin-workspace` still carry.
//!
//! Both crates document their duplicates with `TODO(ws-01)`: they were written
//! before `kevin-domain` landed and will re-export the domain types later. The
//! orchestrator is the only crate that sits on both sides, so the mapping
//! lives here and nowhere else. Every function is total.

use kevin_domain::{
    ArtifactId, ArtifactKind, ArtifactRef, Route, TaskSpec, Usage, Workspace, WorkspaceKind,
    WorkspacePolicy,
};
use kevin_worker::types as wt;
use kevin_workspace::model as wsm;

/// Domain usage from the worker's copy.
#[must_use]
pub fn usage_from_worker(usage: &wt::Usage) -> Usage {
    Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost_usd: usage.cost_usd,
        wall_ms: usage.wall_ms,
    }
}

/// The worker's usage copy from the domain type.
#[must_use]
pub fn usage_to_worker(usage: &Usage) -> wt::Usage {
    wt::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cost_usd: usage.cost_usd,
        wall_ms: usage.wall_ms,
    }
}

/// The worker's workspace copy from the domain type.
#[must_use]
pub fn workspace_to_worker(workspace: &Workspace) -> wt::Workspace {
    wt::Workspace {
        root: workspace.root.clone(),
        kind: match &workspace.kind {
            WorkspaceKind::InPlace => wt::WorkspaceKind::InPlace,
            WorkspaceKind::GitWorktree { branch } => wt::WorkspaceKind::GitWorktree {
                branch: branch.clone(),
            },
            WorkspaceKind::JjWorkspace { name } => {
                wt::WorkspaceKind::JjWorkspace { name: name.clone() }
            }
        },
        base_rev: workspace.base_rev.clone(),
    }
}

/// The domain workspace from `kevin-workspace`'s copy.
#[must_use]
pub fn workspace_from_manager(workspace: &wsm::Workspace) -> Workspace {
    Workspace {
        root: workspace.root.clone(),
        kind: match &workspace.kind {
            wsm::WorkspaceKind::InPlace => WorkspaceKind::InPlace,
            wsm::WorkspaceKind::GitWorktree { branch } => WorkspaceKind::GitWorktree {
                branch: branch.clone(),
            },
            wsm::WorkspaceKind::JjWorkspace { name } => {
                WorkspaceKind::JjWorkspace { name: name.clone() }
            }
        },
        base_rev: workspace.base_rev.clone(),
    }
}

/// `kevin-workspace`'s copy from the domain workspace.
#[must_use]
pub fn workspace_to_manager(workspace: &Workspace) -> wsm::Workspace {
    wsm::Workspace {
        root: workspace.root.clone(),
        kind: match &workspace.kind {
            WorkspaceKind::InPlace => wsm::WorkspaceKind::InPlace,
            WorkspaceKind::GitWorktree { branch } => wsm::WorkspaceKind::GitWorktree {
                branch: branch.clone(),
            },
            WorkspaceKind::JjWorkspace { name } => {
                wsm::WorkspaceKind::JjWorkspace { name: name.clone() }
            }
        },
        base_rev: workspace.base_rev.clone(),
    }
}

/// The plan-level policy as `kevin-workspace` spells it.
#[must_use]
pub const fn policy_to_manager(policy: WorkspacePolicy) -> wsm::WorkspacePolicy {
    match policy {
        WorkspacePolicy::Isolated => wsm::WorkspacePolicy::Isolated,
        WorkspacePolicy::Shared => wsm::WorkspacePolicy::Shared,
        WorkspacePolicy::ReadOnly => wsm::WorkspacePolicy::ReadOnly,
    }
}

/// The plan-level policy as `kevin-worker` spells it (it has no `shared`:
/// a shared workspace is still a writable checkout for the worker).
#[must_use]
pub const fn policy_to_worker(policy: WorkspacePolicy) -> wt::WorkspacePolicy {
    match policy {
        WorkspacePolicy::Isolated | WorkspacePolicy::Shared => wt::WorkspacePolicy::Isolated,
        WorkspacePolicy::ReadOnly => wt::WorkspacePolicy::ReadOnly,
    }
}

/// The worker's route copy from the domain type.
#[must_use]
pub fn route_to_worker(route: &Route) -> wt::Route {
    wt::Route {
        worker: route.worker,
        model: route.model.clone(),
        effort: route.effort,
    }
}

/// The worker's task-spec copy from the domain type (inputs are passed as
/// artifact URIs; the worker only needs the prompt-side fields).
#[must_use]
pub fn spec_to_worker(spec: &TaskSpec) -> wt::TaskSpec {
    wt::TaskSpec {
        title: spec.title.clone(),
        instructions: spec.instructions.clone(),
        inputs: spec.inputs.iter().map(artifact_to_worker).collect(),
        acceptance_criteria: spec.acceptance_criteria.clone(),
        depends_on: spec.depends_on.clone(),
        workspace_policy: policy_to_worker(spec.workspace_policy),
        output_schema: spec.output_schema.clone(),
    }
}

/// The domain artifact from the worker's copy.
#[must_use]
pub fn artifact_from_worker(artifact: &wt::ArtifactRef) -> ArtifactRef {
    ArtifactRef {
        id: ArtifactId::from_uuid(artifact.id),
        kind: artifact_kind_from_worker(artifact.kind),
        uri: artifact.uri.clone(),
        sha256: (!artifact.sha256.is_empty()).then(|| artifact.sha256.clone()),
        bytes: (artifact.bytes > 0).then_some(artifact.bytes),
    }
}

/// The worker's artifact copy from the domain type.
#[must_use]
pub fn artifact_to_worker(artifact: &ArtifactRef) -> wt::ArtifactRef {
    wt::ArtifactRef {
        id: artifact.id.as_uuid(),
        kind: artifact_kind_to_worker(artifact.kind),
        uri: artifact.uri.clone(),
        sha256: artifact.sha256.clone().unwrap_or_default(),
        bytes: artifact.bytes.unwrap_or_default(),
    }
}

/// The domain artifact from `kevin-workspace`'s copy.
#[must_use]
pub fn artifact_from_manager(artifact: &wsm::ArtifactRef) -> ArtifactRef {
    ArtifactRef {
        id: ArtifactId::from_uuid(artifact.id),
        kind: match artifact.kind {
            wsm::ArtifactKind::Diff => ArtifactKind::Diff,
            wsm::ArtifactKind::File => ArtifactKind::File,
            wsm::ArtifactKind::PrUrl => ArtifactKind::PrUrl,
            wsm::ArtifactKind::Report => ArtifactKind::Report,
            wsm::ArtifactKind::Json => ArtifactKind::Json,
            wsm::ArtifactKind::Transcript => ArtifactKind::Transcript,
        },
        uri: artifact.uri.clone(),
        sha256: artifact.sha256.clone(),
        bytes: artifact.bytes,
    }
}

const fn artifact_kind_from_worker(kind: wt::ArtifactKind) -> ArtifactKind {
    match kind {
        wt::ArtifactKind::Diff => ArtifactKind::Diff,
        wt::ArtifactKind::File => ArtifactKind::File,
        wt::ArtifactKind::PrUrl => ArtifactKind::PrUrl,
        wt::ArtifactKind::Report => ArtifactKind::Report,
        wt::ArtifactKind::Json => ArtifactKind::Json,
        wt::ArtifactKind::Transcript => ArtifactKind::Transcript,
    }
}

const fn artifact_kind_to_worker(kind: ArtifactKind) -> wt::ArtifactKind {
    match kind {
        ArtifactKind::Diff => wt::ArtifactKind::Diff,
        ArtifactKind::File => wt::ArtifactKind::File,
        ArtifactKind::PrUrl => wt::ArtifactKind::PrUrl,
        ArtifactKind::Report => wt::ArtifactKind::Report,
        ArtifactKind::Json => wt::ArtifactKind::Json,
        ArtifactKind::Transcript => wt::ArtifactKind::Transcript,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_round_trips_through_the_worker_copy() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 4,
            cache_read_tokens: 1,
            cache_write_tokens: 2,
            cost_usd: Some(rust_decimal::Decimal::new(5, 2)),
            wall_ms: 42,
        };
        assert_eq!(usage_from_worker(&usage_to_worker(&usage)), usage);
    }

    #[test]
    fn workspace_round_trips_through_both_copies() {
        let workspace = Workspace {
            root: "/tmp/ws".into(),
            kind: WorkspaceKind::GitWorktree {
                branch: "kevin/abc/t".into(),
            },
            base_rev: Some("deadbeef".into()),
        };
        assert_eq!(
            workspace_from_manager(&workspace_to_manager(&workspace)),
            workspace
        );
        assert_eq!(workspace_to_worker(&workspace).root, workspace.root.clone());
    }

    #[test]
    fn shared_policy_is_a_writable_checkout_for_the_worker() {
        assert_eq!(
            policy_to_worker(WorkspacePolicy::Shared),
            wt::WorkspacePolicy::Isolated
        );
        assert_eq!(
            policy_to_worker(WorkspacePolicy::ReadOnly),
            wt::WorkspacePolicy::ReadOnly
        );
        assert_eq!(
            policy_to_manager(WorkspacePolicy::Shared),
            wsm::WorkspacePolicy::Shared
        );
    }
}
