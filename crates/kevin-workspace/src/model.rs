//! Value objects of the workspace context.
//!
//! TODO(ws-01): `Workspace`, `WorkspaceKind`, `ArtifactRef`, `ArtifactKind` and
//! `WorkspacePolicy` are specified in `plan/02-domain-model.md` and belong to
//! `kevin-domain`. They are defined here with the exact documented shape until
//! WS-01 lands; afterwards this module re-exports the `kevin-domain` types.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How an attempt's checkout is isolated (`plan/02-domain-model.md`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceKind {
    /// The repository itself (single writing task at a time, or read-only).
    InPlace,
    /// A git worktree checked out on `branch`.
    GitWorktree {
        /// Branch name (`kevin/<run-short>/<task-slug>`).
        branch: String,
    },
    /// A jj workspace named `name`.
    JjWorkspace {
        /// jj workspace name.
        name: String,
    },
}

/// An attempt's checkout: `{ root, kind, base_rev }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Workspace {
    /// Absolute path the worker uses as cwd.
    pub root: PathBuf,
    /// Isolation kind.
    pub kind: WorkspaceKind,
    /// Revision the workspace started from (git sha / jj commit id), when known.
    pub base_rev: Option<String>,
}

impl Workspace {
    /// The git branch when this is a git worktree.
    #[must_use]
    pub fn branch(&self) -> Option<&str> {
        match &self.kind {
            WorkspaceKind::GitWorktree { branch } => Some(branch),
            _ => None,
        }
    }

    /// The jj workspace name when this is a jj workspace.
    #[must_use]
    pub fn jj_name(&self) -> Option<&str> {
        match &self.kind {
            WorkspaceKind::JjWorkspace { name } => Some(name),
            _ => None,
        }
    }

    /// `true` for [`WorkspaceKind::InPlace`].
    #[must_use]
    pub fn is_in_place(&self) -> bool {
        matches!(self.kind, WorkspaceKind::InPlace)
    }
}

/// Plan-level `workspace_policy` of a task (`plan/05-orchestration.md` §plan schema).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspacePolicy {
    /// Own worktree/workspace per attempt (default).
    #[default]
    Isolated,
    /// One workspace shared by the run's `shared` tasks (serialised by the scheduler).
    Shared,
    /// The task does not write; may run in place with read-only worker settings.
    ReadOnly,
}

/// Kind of an [`ArtifactRef`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A unified diff.
    Diff,
    /// An arbitrary file.
    File,
    /// A pull-request URL.
    PrUrl,
    /// A human-readable report (integration uses `branch:<name>` URIs for left branches).
    Report,
    /// Structured JSON.
    Json,
    /// A worker transcript.
    Transcript,
}

/// Reference to a produced artifact: `{ id, kind, uri, sha256, bytes }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Artifact id (uuid v7).
    pub id: uuid::Uuid,
    /// Kind.
    pub kind: ArtifactKind,
    /// Location: `file://…`, `https://…`, `branch:<name>`, `bookmark:<name>`.
    pub uri: String,
    /// Hex sha256 of the content when it was materialised.
    pub sha256: Option<String>,
    /// Size in bytes when known.
    pub bytes: Option<u64>,
}

impl ArtifactRef {
    /// A reference without content hash/size (URLs, branch names).
    pub fn new(kind: ArtifactKind, uri: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::now_v7(),
            kind,
            uri: uri.into(),
            sha256: None,
            bytes: None,
        }
    }
}

impl fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} {}", self.kind, self.uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_kind_serde_shape() {
        let ws = Workspace {
            root: PathBuf::from("/r/.kevin/workspaces/abc/t-1"),
            kind: WorkspaceKind::GitWorktree {
                branch: "kevin/abc/t".into(),
            },
            base_rev: Some("deadbeef".into()),
        };
        let json = serde_json::to_value(&ws).unwrap();
        assert_eq!(json["kind"]["type"], "git_worktree");
        assert_eq!(json["kind"]["branch"], "kevin/abc/t");
        let back: Workspace = serde_json::from_value(json).unwrap();
        assert_eq!(back, ws);
        assert_eq!(back.branch(), Some("kevin/abc/t"));
        assert_eq!(back.jj_name(), None);
    }
}
