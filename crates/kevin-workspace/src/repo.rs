//! Repository kind detection (`plan/05-orchestration.md` §3.1 Intake).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Version-control flavour of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoKind {
    /// A plain git repository (`.git` directory or worktree file).
    Git,
    /// A jj repository (`.jj` directory); a colocated jj+git repo counts as `Jj`.
    Jj,
    /// No version control detected.
    None,
}

impl RepoKind {
    /// Detects the repo kind of `path` itself (no ancestor walk): `.jj` →
    /// [`RepoKind::Jj`] (colocated included), else `.git` (directory **or**
    /// file, i.e. a git worktree) → [`RepoKind::Git`], else [`RepoKind::None`].
    pub fn detect(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        if path.join(".jj").is_dir() {
            RepoKind::Jj
        } else if path.join(".git").exists() {
            RepoKind::Git
        } else {
            RepoKind::None
        }
    }

    /// Walks up from `path` to the closest directory that is a repository and
    /// returns its kind and root. `None` when no ancestor is a repository.
    pub fn locate(path: impl AsRef<Path>) -> Option<(Self, PathBuf)> {
        let mut cur = Some(path.as_ref());
        while let Some(dir) = cur {
            let kind = Self::detect(dir);
            if kind != RepoKind::None {
                return Some((kind, dir.to_path_buf()));
            }
            cur = dir.parent();
        }
        None
    }

    /// `true` unless [`RepoKind::None`].
    #[must_use]
    pub const fn is_vcs(self) -> bool {
        !matches!(self, RepoKind::None)
    }
}

impl std::fmt::Display for RepoKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RepoKind::Git => "git",
            RepoKind::Jj => "jj",
            RepoKind::None => "none",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_none_git_jj_and_colocated() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(RepoKind::detect(dir.path()), RepoKind::None);
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert_eq!(RepoKind::detect(dir.path()), RepoKind::Git);
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        assert_eq!(
            RepoKind::detect(dir.path()),
            RepoKind::Jj,
            "colocated counts as jj"
        );
    }

    #[test]
    fn git_worktree_file_counts_as_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere").unwrap();
        assert_eq!(RepoKind::detect(dir.path()), RepoKind::Git);
    }

    #[test]
    fn locate_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let (kind, root) = RepoKind::locate(&nested).unwrap();
        assert_eq!(kind, RepoKind::Git);
        assert_eq!(root, dir.path());
        let outside = tempfile::tempdir().unwrap();
        assert!(RepoKind::locate(outside.path()).is_none_or(|(_, r)| r != outside.path()));
    }
}
