//! Per-attempt workspace lifecycle (ADR 0007, `plan/09-security.md`
//! §Workspace isolation).
//!
//! Layout: `<repo>/<workspace.root>/<run-short>/<task-slug>-<attempt-short>`
//! (`workspace.root` defaults to `.kevin/workspaces`). `<run-short>` and
//! `<attempt-short>` are the last 8 hex chars of the ids ([`crate::util::short_id`]).
//!
//! | repo | strategy | what `prepare` does |
//! |---|---|---|
//! | jj (colocated or not) | `auto` / `jj_workspace` | `jj workspace add --name <run-short>-<slug>-<attempt-short> <dir>` |
//! | git | `auto` / `git_worktree` | `git worktree add -b <branch_prefix><run-short>/<slug> <dir> HEAD` |
//! | any | `in_place`, or `auto` without VCS | the repository itself; one writing attempt at a time |
//!
//! A task whose plan `workspace_policy` is `read_only` always runs in place
//! (no checkout needed, no write lease). `shared` tasks of a run share one
//! workspace (`<run-short>/shared`). Before the first workspace is created the
//! workspaces directory is added to the repository's local exclude file
//! (`.git/info/exclude` — jj reads the same file of its backing git store), never
//! to `.gitignore`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use kevin_domain::{AttemptId, RunId, TaskId};
use serde::{Deserialize, Serialize};

use crate::cmd::{Cmd, CmdError, CmdOutput, CommandRunner, ProcessRunner};
use crate::config::{CleanupPolicy, Strategy, WorkspaceConfig};
use crate::model::{Workspace, WorkspaceKind, WorkspacePolicy};
use crate::repo::RepoKind;
use crate::util::{canonicalize_lenient, is_within, join_or_absolute, short_id, slugify};

/// Maximum length of a task slug in paths/branches.
const SLUG_MAX: usize = 40;
/// Name of the integration workspace/branch segment (`<run-short>/integration`).
pub(crate) const INTEGRATION_SEGMENT: &str = "integration";
/// Name of the shared workspace segment (`<run-short>/shared`).
const SHARED_SEGMENT: &str = "shared";
/// Suffix of the sidecar describing a workspace (`<run-dir>/.<basename>.meta.json`).
const META_SUFFIX: &str = ".meta.json";

/// Which concrete isolation an attempt gets after applying `strategy = auto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedStrategy {
    /// A git worktree on its own branch.
    GitWorktree,
    /// A jj workspace.
    JjWorkspace,
    /// The repository itself.
    InPlace,
}

/// What `prepare` needs beyond the three ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRequest {
    /// Run the attempt belongs to.
    pub run_id: RunId,
    /// Task the attempt belongs to.
    pub task_id: TaskId,
    /// The attempt.
    pub attempt_id: AttemptId,
    /// Human slug for paths/branches (task title); defaults to `task-<task-short>`.
    pub task_slug: Option<String>,
    /// The plan's `workspace_policy` for the task.
    pub policy: WorkspacePolicy,
}

impl PrepareRequest {
    /// Isolated request with a default slug.
    #[must_use]
    pub fn new(run_id: RunId, task_id: TaskId, attempt_id: AttemptId) -> Self {
        Self {
            run_id,
            task_id,
            attempt_id,
            task_slug: None,
            policy: WorkspacePolicy::Isolated,
        }
    }

    /// Sets the slug source (a task title; slugified).
    #[must_use]
    pub fn with_slug(mut self, slug: impl AsRef<str>) -> Self {
        let s = slugify(slug.as_ref(), SLUG_MAX);
        self.task_slug = (!s.is_empty()).then_some(s);
        self
    }

    /// Sets the workspace policy.
    #[must_use]
    pub fn with_policy(mut self, policy: WorkspacePolicy) -> Self {
        self.policy = policy;
        self
    }

    fn slug(&self) -> String {
        self.task_slug
            .clone()
            .unwrap_or_else(|| format!("task-{}", short_id(self.task_id)))
    }
}

/// Result of [`WorkspaceManager::cleanup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CleanupOutcome {
    /// The worktree/workspace was removed (branch/bookmark kept for integration).
    Removed,
    /// Kept per policy (or in-place: nothing to remove).
    Kept,
}

/// Errors of the workspace manager.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// The repository root does not exist or is not a directory.
    #[error("repository root {path} is not a directory")]
    RepoNotFound {
        /// Offending path.
        path: PathBuf,
    },
    /// The configured strategy cannot be applied to this repository.
    #[error("workspace strategy {strategy:?} is not available for a {repo} repository at {root}")]
    StrategyUnavailable {
        /// Configured strategy.
        strategy: Strategy,
        /// Detected repo kind.
        repo: RepoKind,
        /// Repository root.
        root: PathBuf,
    },
    /// In-place mode admits one writing attempt at a time.
    #[error(
        "in-place workspace is held by attempt {held_by}; in_place allows a single writing task"
    )]
    InPlaceBusy {
        /// The attempt holding the lease.
        held_by: AttemptId,
    },
    /// The repository has no commit yet (nothing to branch from).
    #[error("repository at {root} has no commits; cannot create an isolated workspace")]
    EmptyRepository {
        /// Repository root.
        root: PathBuf,
    },
    /// A VCS command exited non-zero.
    #[error("`{command}` failed with exit code {code}: {stderr}")]
    Command {
        /// Rendered command line.
        command: String,
        /// Exit code.
        code: i32,
        /// stderr tail.
        stderr: String,
    },
    /// A VCS command could not be spawned.
    #[error(transparent)]
    Spawn(#[from] CmdError),
    /// Filesystem failure.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Refused to touch a path outside the workspaces root.
    #[error("refusing to remove {path}: outside the workspaces root {root}")]
    OutsideRoot {
        /// Offending path.
        path: PathBuf,
        /// Workspaces root.
        root: PathBuf,
    },
    /// Metadata sidecar could not be (de)serialised.
    #[error("workspace metadata at {path}: {source}")]
    Meta {
        /// Sidecar path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
}

pub(crate) fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> WorkspaceError + '_ {
    move |source| WorkspaceError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Creates, reuses and removes attempt workspaces for one repository.
#[derive(Debug)]
pub struct WorkspaceManager {
    repo_root: PathBuf,
    repo_kind: RepoKind,
    cfg: WorkspaceConfig,
    runner: Arc<dyn CommandRunner>,
    in_place_lease: Mutex<Option<AttemptId>>,
    excluded: Mutex<bool>,
}

impl WorkspaceManager {
    /// Manager for the repository at `repo_root` using real subprocesses.
    pub fn new(repo_root: impl AsRef<Path>, cfg: WorkspaceConfig) -> Result<Self, WorkspaceError> {
        Self::with_runner(repo_root, cfg, Arc::new(ProcessRunner))
    }

    /// Manager with an injected [`CommandRunner`].
    pub fn with_runner(
        repo_root: impl AsRef<Path>,
        cfg: WorkspaceConfig,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Self, WorkspaceError> {
        let repo_root = repo_root.as_ref();
        if !repo_root.is_dir() {
            return Err(WorkspaceError::RepoNotFound {
                path: repo_root.to_path_buf(),
            });
        }
        let repo_root = repo_root.canonicalize().map_err(io_err(repo_root))?;
        let repo_kind = RepoKind::detect(&repo_root);
        Ok(Self {
            repo_root,
            repo_kind,
            cfg,
            runner,
            in_place_lease: Mutex::new(None),
            excluded: Mutex::new(false),
        })
    }

    /// Canonical repository root.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Repository kind detected at construction.
    #[must_use]
    pub fn repo_kind(&self) -> RepoKind {
        self.repo_kind
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> &WorkspaceConfig {
        &self.cfg
    }

    /// The command runner (shared with an [`crate::Integrator`] if desired).
    #[must_use]
    pub fn runner(&self) -> Arc<dyn CommandRunner> {
        Arc::clone(&self.runner)
    }

    /// `<repo>/<workspace.root>` (absolute roots honoured).
    #[must_use]
    pub fn workspaces_root(&self) -> PathBuf {
        join_or_absolute(&self.repo_root, &self.cfg.root)
    }

    /// `<workspaces_root>/<run-short>`.
    #[must_use]
    pub fn run_dir(&self, run_id: RunId) -> PathBuf {
        self.workspaces_root().join(short_id(run_id))
    }

    /// Applies the `auto` rule (`plan/03`): jj if `.jj`, else git worktree if
    /// `.git`, else in place; explicit strategies must match the repo kind.
    /// `read_only` tasks always resolve to in place.
    pub fn resolve_strategy(
        &self,
        policy: WorkspacePolicy,
    ) -> Result<ResolvedStrategy, WorkspaceError> {
        if policy == WorkspacePolicy::ReadOnly {
            return Ok(ResolvedStrategy::InPlace);
        }
        let unavailable = || WorkspaceError::StrategyUnavailable {
            strategy: self.cfg.strategy,
            repo: self.repo_kind,
            root: self.repo_root.clone(),
        };
        Ok(match (self.cfg.strategy, self.repo_kind) {
            (Strategy::InPlace, _) | (Strategy::Auto, RepoKind::None) => ResolvedStrategy::InPlace,
            (Strategy::Auto | Strategy::JjWorkspace, RepoKind::Jj) => ResolvedStrategy::JjWorkspace,
            (Strategy::Auto | Strategy::GitWorktree, RepoKind::Git) => {
                ResolvedStrategy::GitWorktree
            }
            (Strategy::JjWorkspace | Strategy::GitWorktree, _) => return Err(unavailable()),
        })
    }

    /// Prepares an isolated workspace for `attempt_id` (frozen signature).
    pub fn prepare(
        &self,
        run_id: RunId,
        task_id: TaskId,
        attempt_id: AttemptId,
    ) -> Result<Workspace, WorkspaceError> {
        self.prepare_with(&PrepareRequest::new(run_id, task_id, attempt_id))
    }

    /// Prepares a workspace honouring the task slug and `workspace_policy`.
    pub fn prepare_with(&self, req: &PrepareRequest) -> Result<Workspace, WorkspaceError> {
        let strategy = self.resolve_strategy(req.policy)?;
        tracing::info!(
            run = %req.run_id, task = %req.task_id, attempt = %req.attempt_id,
            ?strategy, policy = ?req.policy, "prepare workspace"
        );
        match strategy {
            ResolvedStrategy::InPlace => self.prepare_in_place(req),
            ResolvedStrategy::GitWorktree | ResolvedStrategy::JjWorkspace => {
                self.ensure_excluded()?;
                let run_short = short_id(req.run_id);
                let basename = match req.policy {
                    WorkspacePolicy::Shared => SHARED_SEGMENT.to_owned(),
                    _ => format!("{}-{}", req.slug(), short_id(req.attempt_id)),
                };
                let dir = self.run_dir(req.run_id).join(&basename);
                if req.policy == WorkspacePolicy::Shared
                    && let Some(existing) = Self::read_meta(&dir)?
                    && existing.root.is_dir()
                {
                    return Ok(existing);
                }
                if let Some(parent) = dir.parent() {
                    fs::create_dir_all(parent).map_err(io_err(parent))?;
                }
                let ws = if strategy == ResolvedStrategy::GitWorktree {
                    let branch_base = match req.policy {
                        WorkspacePolicy::Shared => {
                            format!("{}{run_short}/{SHARED_SEGMENT}", self.cfg.branch_prefix)
                        }
                        _ => format!("{}{run_short}/{}", self.cfg.branch_prefix, req.slug()),
                    };
                    self.create_git_worktree(&dir, &branch_base, req.attempt_id)?
                } else {
                    let name = format!("{run_short}-{basename}");
                    self.create_jj_workspace(&dir, &name)?
                };
                Self::write_meta(&ws)?;
                Ok(ws)
            }
        }
    }

    fn prepare_in_place(&self, req: &PrepareRequest) -> Result<Workspace, WorkspaceError> {
        if req.policy != WorkspacePolicy::ReadOnly {
            let mut lease = self
                .in_place_lease
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(held_by) = *lease
                && held_by != req.attempt_id
            {
                return Err(WorkspaceError::InPlaceBusy { held_by });
            }
            *lease = Some(req.attempt_id);
        }
        let base_rev = match self.repo_kind {
            RepoKind::Git => self.git_head().ok(),
            RepoKind::Jj => self.jj_rev_id(&self.repo_root, "@-").ok(),
            RepoKind::None => None,
        };
        Ok(Workspace {
            root: self.repo_root.clone(),
            kind: WorkspaceKind::InPlace,
            base_rev,
        })
    }

    /// Releases the in-place write lease (also done by `cleanup` of that workspace).
    pub fn release_in_place(&self) {
        *self
            .in_place_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The attempt currently holding the in-place lease.
    #[must_use]
    pub fn in_place_holder(&self) -> Option<AttemptId> {
        *self
            .in_place_lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // -- git ----------------------------------------------------------------

    fn git(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, WorkspaceError> {
        let cmd = Cmd::new("git").args(args.iter().copied()).cwd(cwd);
        Ok(self.runner.run(&cmd)?)
    }

    fn git_ok(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, WorkspaceError> {
        let cmd = Cmd::new("git").args(args.iter().copied()).cwd(cwd);
        expect_ok(&cmd, self.runner.run(&cmd)?)
    }

    fn git_head(&self) -> Result<String, WorkspaceError> {
        let out = self.git(&self.repo_root, &["rev-parse", "--verify", "-q", "HEAD"])?;
        if out.success() {
            Ok(out.stdout.trim().to_owned())
        } else {
            Err(WorkspaceError::EmptyRepository {
                root: self.repo_root.clone(),
            })
        }
    }

    fn git_branch_exists(&self, branch: &str) -> Result<bool, WorkspaceError> {
        let out = self.git(
            &self.repo_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )?;
        Ok(out.success())
    }

    fn create_git_worktree(
        &self,
        dir: &Path,
        branch_base: &str,
        attempt_id: AttemptId,
    ) -> Result<Workspace, WorkspaceError> {
        let head = self.git_head()?;
        // A retry of the same task finds its branch taken (kept worktree or
        // leftover commits): start fresh from HEAD on an attempt-suffixed branch.
        let branch = if self.git_branch_exists(branch_base)? {
            format!("{branch_base}-{}", short_id(attempt_id))
        } else {
            branch_base.to_owned()
        };
        let dir_s = dir.to_string_lossy().into_owned();
        self.git_ok(
            &self.repo_root,
            &["worktree", "add", "--quiet", "-b", &branch, &dir_s, "HEAD"],
        )?;
        Ok(Workspace {
            root: canonicalize_lenient(dir),
            kind: WorkspaceKind::GitWorktree { branch },
            base_rev: Some(head),
        })
    }

    // -- jj -----------------------------------------------------------------

    fn jj(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, WorkspaceError> {
        let cmd = Cmd::new("jj").args(args.iter().copied()).cwd(cwd);
        Ok(self.runner.run(&cmd)?)
    }

    fn jj_ok(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, WorkspaceError> {
        let cmd = Cmd::new("jj").args(args.iter().copied()).cwd(cwd);
        expect_ok(&cmd, self.runner.run(&cmd)?)
    }

    /// Commit id of a revset (first line when it resolves to several).
    pub(crate) fn jj_rev_id(&self, cwd: &Path, revset: &str) -> Result<String, WorkspaceError> {
        let out = self.jj_ok(
            cwd,
            &[
                "log",
                "--no-graph",
                "-r",
                revset,
                "-T",
                "commit_id ++ \"\\n\"",
            ],
        )?;
        Ok(out
            .stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned())
    }

    fn create_jj_workspace(&self, dir: &Path, name: &str) -> Result<Workspace, WorkspaceError> {
        let dir_s = dir.to_string_lossy().into_owned();
        self.jj_ok(
            &self.repo_root,
            &["workspace", "add", "--name", name, &dir_s],
        )?;
        let root = canonicalize_lenient(dir);
        let base_rev = self.jj_rev_id(&root, "@-").ok();
        Ok(Workspace {
            root,
            kind: WorkspaceKind::JjWorkspace {
                name: name.to_owned(),
            },
            base_rev,
        })
    }

    /// Bookmark that names a jj attempt workspace's result once the workspace
    /// is forgotten: `<branch_prefix><workspace-name>`.
    #[must_use]
    pub fn jj_bookmark(&self, ws_name: &str) -> String {
        format!("{}{ws_name}", self.cfg.branch_prefix)
    }

    /// Head commit of a jj workspace: `@` when it has changes, else `@-`.
    /// Snapshots the workspace first so edits made without running `jj` count.
    pub(crate) fn jj_head(&self, ws: &Workspace, name: &str) -> Result<String, WorkspaceError> {
        if ws.root.is_dir() && ws.root.join(".jj").exists() {
            let _ = self.jj(&ws.root, &["status", "--quiet"])?;
            let at = format!("{name}@");
            let empty = self.jj_ok(
                &self.repo_root,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &at,
                    "-T",
                    "if(empty, \"empty\", \"full\")",
                ],
            )?;
            let rev = if empty.stdout.trim() == "empty" {
                format!("{at}-")
            } else {
                at
            };
            return self.jj_rev_id(&self.repo_root, &rev);
        }
        self.jj_rev_id(&self.repo_root, &self.jj_bookmark(name))
    }

    // -- exclude ------------------------------------------------------------

    /// Path of the repository's local exclude file (`.git/info/exclude`, or the
    /// same file of a jj repo's backing git store).
    pub fn exclude_file(&self) -> Option<PathBuf> {
        let dot_git = self.repo_root.join(".git");
        let git_dir = if dot_git.is_dir() {
            Some(dot_git)
        } else if dot_git.is_file() {
            // Linked worktree / submodule: `gitdir: <path>`; `info/` lives in
            // the common dir (`<gitdir>/commondir` when present).
            fs::read_to_string(&dot_git).ok().and_then(|s| {
                let p = s.trim().strip_prefix("gitdir:")?.trim();
                let git_dir = join_or_absolute(&self.repo_root, Path::new(p));
                Some(match fs::read_to_string(git_dir.join("commondir")) {
                    Ok(common) => join_or_absolute(&git_dir, Path::new(common.trim())),
                    Err(_) => git_dir,
                })
            })
        } else if self.repo_kind == RepoKind::Jj {
            let store = self.repo_root.join(".jj").join("repo").join("store");
            let target = fs::read_to_string(store.join("git_target")).ok()?;
            Some(join_or_absolute(&store, Path::new(target.trim())))
        } else {
            None
        }?;
        Some(git_dir.join("info").join("exclude"))
    }

    /// The pattern written to the exclude file (`/<workspace.root>/`), or
    /// `None` when the workspaces root is outside the repository.
    #[must_use]
    pub fn exclude_pattern(&self) -> Option<String> {
        let rel = if self.cfg.root.is_absolute() {
            self.cfg
                .root
                .strip_prefix(&self.repo_root)
                .ok()?
                .to_path_buf()
        } else {
            self.cfg.root.clone()
        };
        let rel = rel.to_string_lossy().trim_matches('/').to_owned();
        (!rel.is_empty()).then(|| format!("/{rel}/"))
    }

    /// Adds the workspaces root to the repository-local exclude file (idempotent).
    /// Returns the exclude file touched, if any.
    pub fn ensure_excluded(&self) -> Result<Option<PathBuf>, WorkspaceError> {
        let mut done = self
            .excluded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (Some(file), Some(pattern)) = (self.exclude_file(), self.exclude_pattern()) else {
            return Ok(None);
        };
        if *done {
            return Ok(Some(file));
        }
        let existing = match fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(WorkspaceError::Io {
                    path: file,
                    source: e,
                });
            }
        };
        let present = existing.lines().map(str::trim).any(|l| {
            l == pattern
                || l == pattern.trim_start_matches('/')
                || l == pattern.trim_end_matches('/')
        });
        if !present {
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent).map_err(io_err(parent))?;
            }
            let mut content = existing;
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("# kevin attempt workspaces (added by kevin-workspace)\n");
            content.push_str(&pattern);
            content.push('\n');
            fs::write(&file, content).map_err(io_err(&file))?;
            tracing::info!(file = %file.display(), %pattern, "added workspaces root to local exclude");
        }
        *done = true;
        Ok(Some(file))
    }

    // -- metadata -----------------------------------------------------------

    fn meta_path(dir: &Path) -> Option<PathBuf> {
        let name = dir.file_name()?.to_string_lossy().into_owned();
        Some(dir.parent()?.join(format!(".{name}{META_SUFFIX}")))
    }

    fn write_meta(ws: &Workspace) -> Result<(), WorkspaceError> {
        let Some(path) = Self::meta_path(&ws.root) else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(ws).map_err(|source| WorkspaceError::Meta {
            path: path.clone(),
            source,
        })?;
        fs::write(&path, json).map_err(io_err(&path))
    }

    fn read_meta(dir: &Path) -> Result<Option<Workspace>, WorkspaceError> {
        let Some(path) = Self::meta_path(dir) else {
            return Ok(None);
        };
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .map(Some)
                .map_err(|source| WorkspaceError::Meta { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io { path, source: e }),
        }
    }

    /// Workspaces recorded for a run (from the metadata sidecars).
    pub fn list_run(&self, run_id: RunId) -> Result<Vec<Workspace>, WorkspaceError> {
        let dir = self.run_dir(run_id);
        let mut out = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => {
                return Err(WorkspaceError::Io {
                    path: dir,
                    source: e,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(io_err(&dir))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(base) = name
                .strip_prefix('.')
                .and_then(|n| n.strip_suffix(META_SUFFIX))
                && let Some(ws) = Self::read_meta(&dir.join(base))?
            {
                out.push(ws);
            }
        }
        out.sort_by(|a, b| a.root.cmp(&b.root));
        Ok(out)
    }

    // -- cleanup ------------------------------------------------------------

    /// Applies `policy` to an attempt workspace. Branches/bookmarks are kept
    /// (integration needs them; see [`WorkspaceManager::discard_branch`]).
    /// Never deletes anything outside the workspaces root.
    pub fn cleanup(
        &self,
        ws: &Workspace,
        policy: CleanupPolicy,
        succeeded: bool,
    ) -> Result<CleanupOutcome, WorkspaceError> {
        if ws.is_in_place() {
            self.release_in_place();
            return Ok(CleanupOutcome::Kept);
        }
        let remove = match policy {
            CleanupPolicy::Always => true,
            CleanupPolicy::OnSuccess => succeeded,
            CleanupPolicy::Never => false,
        };
        if !remove {
            return Ok(CleanupOutcome::Kept);
        }
        self.remove(ws)?;
        Ok(CleanupOutcome::Removed)
    }

    /// Unconditionally removes an attempt workspace (not its branch).
    pub fn remove(&self, ws: &Workspace) -> Result<(), WorkspaceError> {
        self.guard_inside_root(&ws.root)?;
        match &ws.kind {
            WorkspaceKind::InPlace => {
                self.release_in_place();
                return Ok(());
            }
            WorkspaceKind::GitWorktree { .. } => {
                let dir_s = ws.root.to_string_lossy().into_owned();
                let out = self.git(&self.repo_root, &["worktree", "remove", "--force", &dir_s])?;
                if !out.success() && ws.root.exists() {
                    fs::remove_dir_all(&ws.root).map_err(io_err(&ws.root))?;
                    let _ = self.git(&self.repo_root, &["worktree", "prune"])?;
                }
            }
            WorkspaceKind::JjWorkspace { name } => {
                // Name the result before the workspace (and its `name@`) disappears.
                if let Ok(head) = self.jj_head(ws, name) {
                    let bookmark = self.jj_bookmark(name);
                    let _ = self.jj(
                        &self.repo_root,
                        &["bookmark", "set", &bookmark, "-r", &head],
                    )?;
                }
                let _ = self.jj(&self.repo_root, &["workspace", "forget", name])?;
                if ws.root.exists() {
                    fs::remove_dir_all(&ws.root).map_err(io_err(&ws.root))?;
                }
            }
        }
        if let Some(meta) = Self::meta_path(&ws.root)
            && meta.exists()
        {
            fs::remove_file(&meta).map_err(io_err(&meta))?;
        }
        if let Some(run_dir) = ws.root.parent()
            && fs::read_dir(run_dir).is_ok_and(|mut d| d.next().is_none())
        {
            let _ = fs::remove_dir(run_dir);
        }
        tracing::info!(root = %ws.root.display(), "workspace removed");
        Ok(())
    }

    /// Deletes the branch (git) or bookmark (jj) of an attempt workspace, e.g.
    /// after a successful merge. The workspace itself must already be removed.
    pub fn discard_branch(&self, ws: &Workspace) -> Result<(), WorkspaceError> {
        match &ws.kind {
            WorkspaceKind::InPlace => Ok(()),
            WorkspaceKind::GitWorktree { branch } => self
                .git_ok(&self.repo_root, &["branch", "-D", branch])
                .map(|_| ()),
            WorkspaceKind::JjWorkspace { name } => {
                let bookmark = self.jj_bookmark(name);
                let _ = self.jj(&self.repo_root, &["bookmark", "delete", &bookmark])?;
                Ok(())
            }
        }
    }

    /// Removes every recorded workspace of a run and the run directory.
    pub fn cleanup_run(&self, run_id: RunId) -> Result<(), WorkspaceError> {
        for ws in self.list_run(run_id)? {
            self.remove(&ws)?;
        }
        let dir = self.run_dir(run_id);
        if dir.exists() {
            self.guard_inside_root(&dir)?;
            fs::remove_dir_all(&dir).map_err(io_err(&dir))?;
        }
        Ok(())
    }

    fn guard_inside_root(&self, path: &Path) -> Result<(), WorkspaceError> {
        let root = canonicalize_lenient(&self.workspaces_root());
        let candidate = canonicalize_lenient(path);
        if is_within(&root, &candidate) {
            Ok(())
        } else {
            Err(WorkspaceError::OutsideRoot {
                path: path.to_path_buf(),
                root,
            })
        }
    }
}

pub(crate) fn expect_ok(cmd: &Cmd, out: CmdOutput) -> Result<CmdOutput, WorkspaceError> {
    if out.success() {
        Ok(out)
    } else {
        Err(WorkspaceError::Command {
            command: cmd.display(),
            code: out.code,
            stderr: out.tail(4096),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_defaults_to_task_short_id() {
        let task = TaskId::nil();
        let req = PrepareRequest::new(RunId::nil(), task, AttemptId::nil());
        assert_eq!(req.slug(), "task-00000000");
        assert_eq!(req.with_slug("Implement API v2").slug(), "implement-api-v2");
    }

    #[test]
    fn new_rejects_missing_root() {
        let err =
            WorkspaceManager::new("/definitely/not/here", WorkspaceConfig::default()).unwrap_err();
        assert!(matches!(err, WorkspaceError::RepoNotFound { .. }));
    }

    #[test]
    fn auto_without_vcs_is_in_place_and_single_writer() {
        let dir = tempfile::tempdir().unwrap();
        let m = WorkspaceManager::new(dir.path(), WorkspaceConfig::default()).unwrap();
        assert_eq!(m.repo_kind(), RepoKind::None);
        assert_eq!(
            m.resolve_strategy(WorkspacePolicy::Isolated).unwrap(),
            ResolvedStrategy::InPlace
        );
        let a1 = AttemptId::new();
        let ws = m.prepare(RunId::new(), TaskId::new(), a1).unwrap();
        assert!(ws.is_in_place());
        assert_eq!(ws.root, dir.path().canonicalize().unwrap());
        let err = m
            .prepare(RunId::new(), TaskId::new(), AttemptId::new())
            .unwrap_err();
        assert!(matches!(err, WorkspaceError::InPlaceBusy { held_by } if held_by == a1));
        // read-only tasks never need the lease
        let ro = PrepareRequest::new(RunId::new(), TaskId::new(), AttemptId::new())
            .with_policy(WorkspacePolicy::ReadOnly);
        assert!(m.prepare_with(&ro).unwrap().is_in_place());
        assert_eq!(
            m.cleanup(&ws, CleanupPolicy::Always, true).unwrap(),
            CleanupOutcome::Kept
        );
        assert!(m.in_place_holder().is_none());
        assert!(
            dir.path().exists(),
            "in-place cleanup never deletes the repo"
        );
    }

    #[test]
    fn explicit_strategy_must_match_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = WorkspaceConfig {
            strategy: Strategy::GitWorktree,
            ..WorkspaceConfig::default()
        };
        let m = WorkspaceManager::new(dir.path(), cfg).unwrap();
        assert!(matches!(
            m.resolve_strategy(WorkspacePolicy::Isolated),
            Err(WorkspaceError::StrategyUnavailable { .. })
        ));
        let cfg = WorkspaceConfig {
            strategy: Strategy::JjWorkspace,
            ..WorkspaceConfig::default()
        };
        fs::create_dir(dir.path().join(".git")).unwrap();
        let m = WorkspaceManager::new(dir.path(), cfg).unwrap();
        assert!(matches!(
            m.resolve_strategy(WorkspacePolicy::Isolated),
            Err(WorkspaceError::StrategyUnavailable { .. })
        ));
    }

    #[test]
    fn exclude_pattern_and_file_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let m = WorkspaceManager::new(dir.path(), WorkspaceConfig::default()).unwrap();
        assert_eq!(m.exclude_pattern().as_deref(), Some("/.kevin/workspaces/"));
        assert_eq!(
            m.exclude_file().unwrap(),
            dir.path().canonicalize().unwrap().join(".git/info/exclude")
        );
        let outside = WorkspaceManager::new(
            dir.path(),
            WorkspaceConfig {
                root: PathBuf::from("/var/tmp/elsewhere"),
                ..WorkspaceConfig::default()
            },
        )
        .unwrap();
        assert_eq!(outside.exclude_pattern(), None);
    }

    #[test]
    fn exclude_file_of_a_linked_worktree_is_in_the_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path().join("main").join(".git");
        let wt_git = common.join("worktrees").join("wt");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = dir.path().join("wt");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let m = WorkspaceManager::new(&wt, WorkspaceConfig::default()).unwrap();
        assert_eq!(m.repo_kind(), RepoKind::Git);
        let file = m.exclude_file().unwrap();
        assert_eq!(
            canonicalize_lenient(&file),
            canonicalize_lenient(&common.join("info").join("exclude"))
        );
    }

    #[test]
    fn remove_refuses_paths_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let m = WorkspaceManager::new(dir.path(), WorkspaceConfig::default()).unwrap();
        let victim = tempfile::tempdir().unwrap();
        let ws = Workspace {
            root: victim.path().to_path_buf(),
            kind: WorkspaceKind::GitWorktree { branch: "x".into() },
            base_rev: None,
        };
        assert!(matches!(
            m.remove(&ws),
            Err(WorkspaceError::OutsideRoot { .. })
        ));
        assert!(victim.path().exists());
        // the workspaces root itself is not "inside" the root either
        let ws = Workspace {
            root: m.workspaces_root(),
            kind: WorkspaceKind::GitWorktree { branch: "x".into() },
            base_rev: None,
        };
        assert!(matches!(
            m.remove(&ws),
            Err(WorkspaceError::OutsideRoot { .. })
        ));
    }
}
