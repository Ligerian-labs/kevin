//! Integration of succeeded task workspaces (`plan/05-orchestration.md` §3.6,
//! `plan/09-security.md` §Workspace isolation).
//!
//! - `pr`: merge every task branch onto a fresh integration branch
//!   (`<branch_prefix><run-short>/integration`) in its own worktree/workspace,
//!   run the repo's `[checks]`, push, `gh pr create` with the acceptance
//!   criteria in the body; artifacts = PR URL (+ final diff). `pr_per_task`
//!   pushes each task branch and opens one PR per task instead.
//! - `merge`: same merging, then the base branch is moved to the integrated
//!   result locally.
//! - `none`: branches/bookmarks are left; artifacts = their names.
//!
//! Merge conflicts are **reported, never resolved**: the integrator returns
//! [`IntegrationResult::conflicts`] (one entry per conflicting branch with the
//! file list) and leaves the integration workspace in place for an
//! `Integrate` task. Pushing and PR creation are done by Kevin (this crate),
//! never by workers.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kevin_domain::RunId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cmd::{Cmd, CmdError, CmdOutput, CommandRunner, ProcessRunner};
use crate::config::{ChecksConfig, IntegrationMode, KevinConfig, WorkspaceConfig};
use crate::model::{ArtifactKind, ArtifactRef, Workspace, WorkspaceKind};
use crate::repo::RepoKind;
use crate::util::{canonicalize_lenient, join_or_absolute, short_id};
use crate::workspace::{INTEGRATION_SEGMENT, WorkspaceError, WorkspaceManager, io_err};

/// How the integrator behaves (derived from `[workspace]` + `[checks]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationConfig {
    /// `workspace.branch_prefix`.
    pub branch_prefix: String,
    /// `workspace.root` (integration worktree lives under it).
    pub workspaces_root: PathBuf,
    /// `workspace.pr_per_task`.
    pub pr_per_task: bool,
    /// `checks.commands`, run with `sh -c` in the integration workspace before a PR.
    pub checks: Vec<String>,
    /// Git remote to push to.
    pub remote: String,
    /// Where the final diff is written (`<artifacts_dir>/<run-id>/integration.diff`);
    /// `None` skips the diff artifact.
    pub artifacts_dir: Option<PathBuf>,
    /// `gh` binary.
    pub gh_bin: String,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self::from_workspace(&WorkspaceConfig::default(), &ChecksConfig::default())
    }
}

impl IntegrationConfig {
    /// Derives the integrator settings from a full `KevinConfig`.
    #[must_use]
    pub fn from_config(cfg: &KevinConfig) -> Self {
        Self::from_workspace(&cfg.workspace, &cfg.checks)
    }

    /// Derives the integrator settings from the config sections.
    #[must_use]
    pub fn from_workspace(ws: &WorkspaceConfig, checks: &ChecksConfig) -> Self {
        Self {
            branch_prefix: ws.branch_prefix.clone(),
            workspaces_root: ws.root.clone(),
            pr_per_task: ws.pr_per_task,
            checks: checks.commands.clone(),
            remote: "origin".to_owned(),
            artifacts_dir: None,
            gh_bin: "gh".to_owned(),
        }
    }
}

/// What the integrator knows about the run (title, criteria, base).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationRun {
    /// The run.
    pub run_id: RunId,
    /// PR title / merge subject.
    pub title: String,
    /// PR body summary (goal, what was done).
    pub summary: String,
    /// Acceptance criteria from the **approved plan** (never from worker claims).
    pub acceptance_criteria: Vec<String>,
    /// Base branch/bookmark; `None` = the branch checked out at the repo root
    /// (git) or `main`/`master`/`trunk()` (jj).
    pub base_branch: Option<String>,
}

impl IntegrationRun {
    /// Run description with no criteria and automatic base.
    pub fn new(run_id: RunId, title: impl Into<String>) -> Self {
        Self {
            run_id,
            title: title.into(),
            summary: String::new(),
            acceptance_criteria: Vec::new(),
            base_branch: None,
        }
    }
}

/// A merge conflict between one source and the integration branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// Task branch (git) or workspace name (jj) that could not be merged.
    pub source: String,
    /// Conflicting paths (relative to the repo root).
    pub files: Vec<String>,
    /// VCS message tail.
    pub detail: String,
}

/// Outcome of [`Integrator::integrate`]: `{ artifacts, conflicts }` (+ where
/// the integration happened).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntegrationResult {
    /// PR URLs, diffs, branch names.
    pub artifacts: Vec<ArtifactRef>,
    /// Unresolved conflicts (empty when clean).
    pub conflicts: Vec<Conflict>,
    /// Integration worktree/workspace left for an `Integrate` task when
    /// conflicts exist (removed otherwise).
    pub integration_workspace: Option<Workspace>,
    /// The branch/bookmark carrying the integrated result (`pr`: the pushed
    /// integration branch; `merge`: the base branch).
    pub integration_branch: Option<String>,
}

impl IntegrationResult {
    /// `true` when no conflict was reported.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }

    /// PR URLs among the artifacts.
    pub fn pr_urls(&self) -> impl Iterator<Item = &str> {
        self.artifacts
            .iter()
            .filter(|a| a.kind == ArtifactKind::PrUrl)
            .map(|a| a.uri.as_str())
    }
}

/// Integration failures (conflicts are **not** errors; see [`IntegrationResult`]).
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    /// Nothing to integrate.
    #[error("no workspaces to integrate")]
    NoWorkspaces,
    /// No base branch given and none detectable.
    #[error("cannot determine the base branch of {root}; set IntegrationRun.base_branch")]
    NoBaseBranch {
        /// Repository root.
        root: PathBuf,
    },
    /// The repository has no VCS.
    #[error("repository at {root} has no version control; integration needs git or jj")]
    NoVcs {
        /// Repository root.
        root: PathBuf,
    },
    /// Workspace kind does not fit the repository (e.g. a jj workspace in a git integrator).
    #[error("workspace {root} ({kind:?}) cannot be integrated into a {repo} repository")]
    UnsupportedWorkspace {
        /// Workspace root.
        root: PathBuf,
        /// Its kind.
        kind: WorkspaceKind,
        /// Repository kind.
        repo: RepoKind,
    },
    /// A repo check failed.
    #[error("check `{command}` failed with exit code {code}: {output}")]
    ChecksFailed {
        /// The command.
        command: String,
        /// Exit code.
        code: i32,
        /// Output tail.
        output: String,
    },
    /// Pushing failed.
    #[error("push of `{branch}` to `{remote}` failed: {stderr}")]
    PushFailed {
        /// Branch.
        branch: String,
        /// Remote.
        remote: String,
        /// stderr tail.
        stderr: String,
    },
    /// `gh pr create` failed.
    #[error("`gh pr create` failed with exit code {code}: {stderr}")]
    PrCreateFailed {
        /// Exit code.
        code: i32,
        /// stderr tail.
        stderr: String,
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
    /// A command could not be spawned.
    #[error(transparent)]
    Spawn(#[from] CmdError),
    /// Workspace-manager failure (integration workspace handling).
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// Filesystem failure.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl From<(PathBuf, std::io::Error)> for IntegrationError {
    fn from((path, source): (PathBuf, std::io::Error)) -> Self {
        Self::Io { path, source }
    }
}

/// Merges/pushes/opens PRs for the succeeded workspaces of a run.
#[derive(Debug)]
pub struct Integrator {
    repo_root: PathBuf,
    repo_kind: RepoKind,
    cfg: IntegrationConfig,
    runner: Arc<dyn CommandRunner>,
    manager: WorkspaceManager,
}

impl Integrator {
    /// Integrator for the repository at `repo_root` using real subprocesses.
    pub fn new(
        repo_root: impl AsRef<Path>,
        cfg: IntegrationConfig,
    ) -> Result<Self, IntegrationError> {
        Self::with_runner(repo_root, cfg, Arc::new(ProcessRunner))
    }

    /// Integrator with an injected [`CommandRunner`] (tests stub `gh`/`git push`).
    pub fn with_runner(
        repo_root: impl AsRef<Path>,
        cfg: IntegrationConfig,
        runner: Arc<dyn CommandRunner>,
    ) -> Result<Self, IntegrationError> {
        let ws_cfg = WorkspaceConfig {
            branch_prefix: cfg.branch_prefix.clone(),
            root: cfg.workspaces_root.clone(),
            ..WorkspaceConfig::default()
        };
        let manager = WorkspaceManager::with_runner(repo_root, ws_cfg, Arc::clone(&runner))?;
        Ok(Self {
            repo_root: manager.repo_root().to_path_buf(),
            repo_kind: manager.repo_kind(),
            cfg,
            runner,
            manager,
        })
    }

    /// Canonical repository root.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Integration branch/bookmark name for a run.
    #[must_use]
    pub fn integration_branch(&self, run_id: RunId) -> String {
        format!(
            "{}{}/{INTEGRATION_SEGMENT}",
            self.cfg.branch_prefix,
            short_id(run_id)
        )
    }

    /// Renders the PR body: summary, acceptance criteria checklist, sources.
    #[must_use]
    pub fn pr_body(run: &IntegrationRun, sources: &[String]) -> String {
        let mut body = String::new();
        body.push_str("## Summary\n");
        if run.summary.trim().is_empty() {
            body.push_str(&run.title);
        } else {
            body.push_str(run.summary.trim());
        }
        body.push_str("\n\n## Acceptance criteria\n");
        if run.acceptance_criteria.is_empty() {
            body.push_str("_none recorded_\n");
        }
        for c in &run.acceptance_criteria {
            body.push_str("- [ ] ");
            body.push_str(c.trim());
            body.push('\n');
        }
        if !sources.is_empty() {
            body.push_str("\n## Integrated branches\n");
            for s in sources {
                body.push_str("- `");
                body.push_str(s);
                body.push_str("`\n");
            }
        }
        let _ = write!(body, "\n---\nKevin run `{}`\n", run.run_id);
        body
    }

    /// Integrates `workspaces` per `mode` (frozen signature).
    pub fn integrate(
        &self,
        run: &IntegrationRun,
        workspaces: &[Workspace],
        mode: IntegrationMode,
    ) -> Result<IntegrationResult, IntegrationError> {
        let sources: Vec<&Workspace> = workspaces.iter().filter(|w| !w.is_in_place()).collect();
        if sources.is_empty() {
            return Err(IntegrationError::NoWorkspaces);
        }
        for ws in &sources {
            let ok = matches!(
                (self.repo_kind, &ws.kind),
                (RepoKind::Git, WorkspaceKind::GitWorktree { .. })
                    | (RepoKind::Jj, WorkspaceKind::JjWorkspace { .. })
            );
            if !ok {
                return Err(IntegrationError::UnsupportedWorkspace {
                    root: ws.root.clone(),
                    kind: ws.kind.clone(),
                    repo: self.repo_kind,
                });
            }
        }
        tracing::info!(run = %run.run_id, ?mode, sources = sources.len(), repo = %self.repo_kind, "integrate");
        match self.repo_kind {
            RepoKind::Git => self.integrate_git(run, &sources, mode),
            RepoKind::Jj => self.integrate_jj(run, &sources, mode),
            RepoKind::None => Err(IntegrationError::NoVcs {
                root: self.repo_root.clone(),
            }),
        }
    }

    // -- shared helpers -----------------------------------------------------

    fn run_cmd(&self, cmd: &Cmd) -> Result<CmdOutput, IntegrationError> {
        Ok(self.runner.run(cmd)?)
    }

    fn run_ok(&self, cmd: &Cmd) -> Result<CmdOutput, IntegrationError> {
        let out = self.run_cmd(cmd)?;
        if out.success() {
            Ok(out)
        } else {
            Err(IntegrationError::Command {
                command: cmd.display(),
                code: out.code,
                stderr: out.tail(4096),
            })
        }
    }

    fn integration_dir(&self, run_id: RunId) -> PathBuf {
        self.manager.run_dir(run_id).join(INTEGRATION_SEGMENT)
    }

    fn run_checks(&self, cwd: &Path) -> Result<(), IntegrationError> {
        for command in &self.cfg.checks {
            tracing::info!(%command, cwd = %cwd.display(), "integration check");
            let out = self.run_cmd(&Cmd::new("sh").args(["-c", command]).cwd(cwd))?;
            if !out.success() {
                return Err(IntegrationError::ChecksFailed {
                    command: command.clone(),
                    code: out.code,
                    output: out.tail(4096),
                });
            }
        }
        Ok(())
    }

    fn diff_artifact(
        &self,
        run_id: RunId,
        diff: &str,
    ) -> Result<Option<ArtifactRef>, IntegrationError> {
        let Some(dir) = &self.cfg.artifacts_dir else {
            return Ok(None);
        };
        let dir = join_or_absolute(&self.repo_root, dir).join(run_id.to_string());
        fs::create_dir_all(&dir).map_err(|e| (dir.clone(), e))?;
        let path = dir.join("integration.diff");
        fs::write(&path, diff).map_err(|e| (path.clone(), e))?;
        let mut artifact = ArtifactRef::new(
            ArtifactKind::Diff,
            format!("file://{}", canonicalize_lenient(&path).display()),
        );
        artifact.sha256 = Some(format!("{:x}", Sha256::digest(diff.as_bytes())));
        artifact.bytes = Some(diff.len() as u64);
        Ok(Some(artifact))
    }

    fn gh_pr_create(
        &self,
        base: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<ArtifactRef, IntegrationError> {
        let cmd = Cmd::new(&self.cfg.gh_bin)
            .args([
                "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body,
            ])
            .cwd(&self.repo_root);
        let out = self.run_cmd(&cmd)?;
        if !out.success() {
            return Err(IntegrationError::PrCreateFailed {
                code: out.code,
                stderr: out.tail(4096),
            });
        }
        let url = out
            .stdout
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("https://") || l.starts_with("http://"))
            .unwrap_or_else(|| out.stdout.trim())
            .to_owned();
        tracing::info!(%url, head, base, "pull request created");
        Ok(ArtifactRef::new(ArtifactKind::PrUrl, url))
    }

    // -- git ----------------------------------------------------------------

    fn git(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, IntegrationError> {
        self.run_cmd(&Cmd::new("git").args(args.iter().copied()).cwd(cwd))
    }

    fn git_ok(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, IntegrationError> {
        self.run_ok(&Cmd::new("git").args(args.iter().copied()).cwd(cwd))
    }

    fn git_current_branch(&self) -> Result<Option<String>, IntegrationError> {
        let out = self.git(&self.repo_root, &["symbolic-ref", "--short", "-q", "HEAD"])?;
        Ok(out
            .success()
            .then(|| out.stdout.trim().to_owned())
            .filter(|s| !s.is_empty()))
    }

    fn git_base(&self, run: &IntegrationRun) -> Result<String, IntegrationError> {
        if let Some(b) = &run.base_branch {
            return Ok(b.clone());
        }
        self.git_current_branch()?
            .ok_or_else(|| IntegrationError::NoBaseBranch {
                root: self.repo_root.clone(),
            })
    }

    fn git_push(&self, branch: &str) -> Result<(), IntegrationError> {
        let out = self.git(&self.repo_root, &["push", "-u", &self.cfg.remote, branch])?;
        if out.success() {
            Ok(())
        } else {
            Err(IntegrationError::PushFailed {
                branch: branch.to_owned(),
                remote: self.cfg.remote.clone(),
                stderr: out.tail(4096),
            })
        }
    }

    fn git_remove_integration(&self, dir: &Path, branch: &str) -> Result<(), IntegrationError> {
        if dir.exists() {
            let ws = Workspace {
                root: dir.to_path_buf(),
                kind: WorkspaceKind::GitWorktree {
                    branch: branch.to_owned(),
                },
                base_rev: None,
            };
            self.manager.remove(&ws)?;
        } else {
            let _ = self.git(&self.repo_root, &["worktree", "prune"])?;
        }
        let _ = self.git(&self.repo_root, &["branch", "-D", branch])?;
        Ok(())
    }

    fn integrate_git(
        &self,
        run: &IntegrationRun,
        sources: &[&Workspace],
        mode: IntegrationMode,
    ) -> Result<IntegrationResult, IntegrationError> {
        let branches: Vec<String> = sources
            .iter()
            .filter_map(|w| w.branch().map(str::to_owned))
            .collect();
        let mut result = IntegrationResult::default();
        if mode == IntegrationMode::None {
            result.artifacts = branches
                .iter()
                .map(|b| ArtifactRef::new(ArtifactKind::Report, format!("branch:{b}")))
                .collect();
            return Ok(result);
        }
        let base = self.git_base(run)?;
        self.git_ok(
            &self.repo_root,
            &["rev-parse", "--verify", "-q", &format!("refs/heads/{base}")],
        )?;

        if mode == IntegrationMode::Pr && self.cfg.pr_per_task {
            for b in &branches {
                self.git_push(b)?;
                let title = format!("{}: {b}", run.title);
                let body = Self::pr_body(run, std::slice::from_ref(b));
                result
                    .artifacts
                    .push(self.gh_pr_create(&base, b, &title, &body)?);
            }
            return Ok(result);
        }

        // Fresh integration worktree on its own branch, from base.
        self.manager.ensure_excluded()?;
        let ibranch = self.integration_branch(run.run_id);
        let idir = self.integration_dir(run.run_id);
        self.git_remove_integration(&idir, &ibranch)?;
        if let Some(parent) = idir.parent() {
            fs::create_dir_all(parent).map_err(|e| (parent.to_path_buf(), e))?;
        }
        let idir_s = idir.to_string_lossy().into_owned();
        self.git_ok(
            &self.repo_root,
            &["worktree", "add", "--quiet", "-b", &ibranch, &idir_s, &base],
        )?;
        let idir = canonicalize_lenient(&idir);
        let iws = Workspace {
            root: idir.clone(),
            kind: WorkspaceKind::GitWorktree {
                branch: ibranch.clone(),
            },
            base_rev: Some(
                self.git_ok(&self.repo_root, &["rev-parse", &base])?
                    .stdout
                    .trim()
                    .to_owned(),
            ),
        };

        for b in &branches {
            let msg = format!("kevin: merge {b}");
            let out = self.git(&idir, &["merge", "--no-ff", "--no-edit", "-m", &msg, b])?;
            if out.success() {
                continue;
            }
            let files = self
                .git(&idir, &["diff", "--name-only", "--diff-filter=U"])?
                .stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect();
            let _ = self.git(&idir, &["merge", "--abort"])?;
            tracing::warn!(branch = %b, "merge conflict reported, not resolved");
            result.conflicts.push(Conflict {
                source: b.clone(),
                files,
                detail: out.tail(2048),
            });
        }
        if !result.conflicts.is_empty() {
            result.integration_workspace = Some(iws);
            result.integration_branch = Some(ibranch);
            return Ok(result);
        }

        self.run_checks(&idir)?;
        let diff = self
            .git_ok(&idir, &["diff", &format!("{base}...{ibranch}")])?
            .stdout;
        result
            .artifacts
            .extend(self.diff_artifact(run.run_id, &diff)?);

        match mode {
            IntegrationMode::Merge => {
                if self.git_current_branch()?.as_deref() == Some(base.as_str()) {
                    self.git_ok(&self.repo_root, &["merge", "--ff-only", &ibranch])?;
                } else {
                    self.git_ok(&self.repo_root, &["branch", "-f", &base, &ibranch])?;
                }
                self.git_remove_integration(&idir, &ibranch)?;
                result.artifacts.push(ArtifactRef::new(
                    ArtifactKind::Report,
                    format!("branch:{base}"),
                ));
                result.integration_branch = Some(base);
            }
            IntegrationMode::Pr => {
                self.git_push(&ibranch)?;
                let body = Self::pr_body(run, &branches);
                result
                    .artifacts
                    .push(self.gh_pr_create(&base, &ibranch, &run.title, &body)?);
                // The branch is pushed; the worktree is no longer needed.
                self.manager.remove(&iws)?;
                result.integration_branch = Some(ibranch);
            }
            IntegrationMode::None => unreachable!("handled above"),
        }
        Ok(result)
    }

    // -- jj -----------------------------------------------------------------

    fn jj(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, IntegrationError> {
        self.run_cmd(&Cmd::new("jj").args(args.iter().copied()).cwd(cwd))
    }

    fn jj_ok(&self, cwd: &Path, args: &[&str]) -> Result<CmdOutput, IntegrationError> {
        self.run_ok(&Cmd::new("jj").args(args.iter().copied()).cwd(cwd))
    }

    fn jj_template(
        &self,
        cwd: &Path,
        revset: &str,
        template: &str,
    ) -> Result<String, IntegrationError> {
        let out = self.jj_ok(cwd, &["log", "--no-graph", "-r", revset, "-T", template])?;
        Ok(out.stdout.trim().to_owned())
    }

    fn jj_base(&self, run: &IntegrationRun) -> Result<String, IntegrationError> {
        if let Some(b) = &run.base_branch {
            return Ok(b.clone());
        }
        let names = self.jj_ok(
            &self.repo_root,
            &["bookmark", "list", "-T", "name ++ \"\\n\""],
        )?;
        let names: Vec<&str> = names.stdout.lines().map(str::trim).collect();
        for candidate in ["main", "master", "trunk"] {
            if names.contains(&candidate) {
                return Ok(candidate.to_owned());
            }
        }
        let trunk = self.jj_template(
            &self.repo_root,
            "trunk()",
            "bookmarks.map(|b| b.name()).join(\" \")",
        )?;
        trunk
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| IntegrationError::NoBaseBranch {
                root: self.repo_root.clone(),
            })
    }

    fn jj_push(&self, bookmark: &str) -> Result<(), IntegrationError> {
        let out = self.jj(
            &self.repo_root,
            &[
                "git",
                "push",
                "--remote",
                &self.cfg.remote,
                "-b",
                bookmark,
                "--allow-new",
            ],
        )?;
        if out.success() {
            Ok(())
        } else {
            Err(IntegrationError::PushFailed {
                branch: bookmark.to_owned(),
                remote: self.cfg.remote.clone(),
                stderr: out.tail(4096),
            })
        }
    }

    fn jj_remove_integration(&self, dir: &Path, name: &str) -> Result<(), IntegrationError> {
        let _ = self.jj(&self.repo_root, &["workspace", "forget", name])?;
        if dir.exists() {
            let ws = Workspace {
                root: dir.to_path_buf(),
                kind: WorkspaceKind::JjWorkspace {
                    name: name.to_owned(),
                },
                base_rev: None,
            };
            // `remove` would try to bookmark the head; the integration workspace
            // must not leave a stray bookmark, so delete the directory directly
            // after the containment check the manager performs.
            self.manager.remove(&ws).or_else(|e| match e {
                WorkspaceError::OutsideRoot { .. } => Err(e),
                _ => fs::remove_dir_all(dir).map_err(io_err(dir)),
            })?;
            let _ = self.jj(
                &self.repo_root,
                &["bookmark", "delete", &self.manager.jj_bookmark(name)],
            )?;
        }
        Ok(())
    }

    fn integrate_jj(
        &self,
        run: &IntegrationRun,
        sources: &[&Workspace],
        mode: IntegrationMode,
    ) -> Result<IntegrationResult, IntegrationError> {
        let mut result = IntegrationResult::default();
        // Resolve every source head (snapshotting live workspaces) and name it.
        let mut heads: Vec<(String, String)> = Vec::new(); // (workspace name, commit id)
        for ws in sources {
            let name = ws.jj_name().unwrap_or_default().to_owned();
            let head = self.manager.jj_head(ws, &name)?;
            let bookmark = self.manager.jj_bookmark(&name);
            self.jj_ok(
                &self.repo_root,
                &["bookmark", "set", &bookmark, "-r", &head],
            )?;
            heads.push((name, head));
        }
        if mode == IntegrationMode::None {
            result.artifacts = heads
                .iter()
                .map(|(n, _)| {
                    ArtifactRef::new(
                        ArtifactKind::Report,
                        format!("bookmark:{}", self.manager.jj_bookmark(n)),
                    )
                })
                .collect();
            return Ok(result);
        }
        let base = self.jj_base(run)?;
        let base_id = self.jj_template(&self.repo_root, &base, "commit_id")?;
        if base_id.is_empty() {
            return Err(IntegrationError::NoBaseBranch {
                root: self.repo_root.clone(),
            });
        }

        if mode == IntegrationMode::Pr && self.cfg.pr_per_task {
            for (name, _) in &heads {
                let bookmark = self.manager.jj_bookmark(name);
                self.jj_push(&bookmark)?;
                let title = format!("{}: {bookmark}", run.title);
                let body = Self::pr_body(run, std::slice::from_ref(&bookmark));
                result
                    .artifacts
                    .push(self.gh_pr_create(&base, &bookmark, &title, &body)?);
            }
            return Ok(result);
        }

        self.manager.ensure_excluded()?;
        let iname = format!("{}-{INTEGRATION_SEGMENT}", short_id(run.run_id));
        let ibranch = self.integration_branch(run.run_id);
        let idir = self.integration_dir(run.run_id);
        self.jj_remove_integration(&idir, &iname)?;
        if let Some(parent) = idir.parent() {
            fs::create_dir_all(parent).map_err(|e| (parent.to_path_buf(), e))?;
        }
        let idir_s = idir.to_string_lossy().into_owned();
        self.jj_ok(
            &self.repo_root,
            &[
                "workspace",
                "add",
                "--name",
                &iname,
                "-r",
                &base_id,
                &idir_s,
            ],
        )?;
        let idir = canonicalize_lenient(&idir);
        let iws = Workspace {
            root: idir.clone(),
            kind: WorkspaceKind::JjWorkspace {
                name: iname.clone(),
            },
            base_rev: Some(base_id.clone()),
        };

        let mut cur = base_id.clone();
        for (name, head) in &heads {
            if head == &cur {
                continue;
            }
            let msg = format!("kevin: merge {name}");
            self.jj_ok(&idir, &["new", &cur, head, "-m", &msg])?;
            let merge_id = self.jj_template(&idir, "@", "commit_id")?;
            let conflicted = self.jj_template(&idir, "@", "if(conflict, \"true\", \"false\")")?;
            if conflicted == "true" {
                let files = self
                    .jj(&idir, &["resolve", "--list"])?
                    .stdout
                    .lines()
                    .filter_map(|l| l.split_whitespace().next())
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                // Park the working copy on the last good state, drop the merge.
                self.jj_ok(&idir, &["new", &cur])?;
                self.jj_ok(&idir, &["abandon", &merge_id])?;
                tracing::warn!(workspace = %name, "merge conflict reported, not resolved");
                result.conflicts.push(Conflict {
                    source: name.clone(),
                    files,
                    detail: format!("jj new {cur} {head} produced a conflicted commit"),
                });
            } else {
                cur = merge_id;
            }
        }
        if !result.conflicts.is_empty() {
            result.integration_workspace = Some(iws);
            result.integration_branch = Some(ibranch);
            return Ok(result);
        }

        // Working copy = empty child of the integrated result, files checked out.
        self.jj_ok(&idir, &["new", &cur])?;
        self.run_checks(&idir)?;
        let diff = self
            .jj_ok(&idir, &["diff", "--git", "--from", &base_id, "--to", &cur])?
            .stdout;
        result
            .artifacts
            .extend(self.diff_artifact(run.run_id, &diff)?);

        match mode {
            IntegrationMode::Merge => {
                self.jj_ok(&self.repo_root, &["bookmark", "set", &base, "-r", &cur])?;
                self.jj_remove_integration(&idir, &iname)?;
                result.artifacts.push(ArtifactRef::new(
                    ArtifactKind::Report,
                    format!("bookmark:{base}"),
                ));
                result.integration_branch = Some(base);
            }
            IntegrationMode::Pr => {
                self.jj_ok(&self.repo_root, &["bookmark", "set", &ibranch, "-r", &cur])?;
                self.jj_push(&ibranch)?;
                let names: Vec<String> = heads
                    .iter()
                    .map(|(n, _)| self.manager.jj_bookmark(n))
                    .collect();
                let body = Self::pr_body(run, &names);
                result
                    .artifacts
                    .push(self.gh_pr_create(&base, &ibranch, &run.title, &body)?);
                self.jj_remove_integration(&idir, &iname)?;
                result.integration_branch = Some(ibranch);
            }
            IntegrationMode::None => unreachable!("handled above"),
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_body_lists_criteria_as_checklist() {
        let run = IntegrationRun {
            run_id: RunId::nil(),
            title: "Add API".into(),
            summary: "Implements the API.".into(),
            acceptance_criteria: vec!["returns 200".into(), "has tests".into()],
            base_branch: None,
        };
        let body = Integrator::pr_body(&run, &["kevin/x/api".to_owned()]);
        assert!(body.contains("## Acceptance criteria\n- [ ] returns 200\n- [ ] has tests\n"));
        assert!(body.contains("- `kevin/x/api`"));
        assert!(body.starts_with("## Summary\nImplements the API."));
    }

    #[test]
    fn integration_branch_uses_prefix_and_short_run() {
        let dir = tempfile::tempdir().unwrap();
        let i = Integrator::new(dir.path(), IntegrationConfig::default()).unwrap();
        let run = RunId::from_uuid(
            uuid::Uuid::parse_str("01910000-0000-7000-8000-0000deadbeef").unwrap(),
        );
        assert_eq!(i.integration_branch(run), "kevin/deadbeef/integration");
    }

    #[test]
    fn no_vcs_repo_cannot_integrate() {
        let dir = tempfile::tempdir().unwrap();
        let i = Integrator::new(dir.path(), IntegrationConfig::default()).unwrap();
        let run = IntegrationRun::new(RunId::new(), "t");
        assert!(matches!(
            i.integrate(&run, &[], IntegrationMode::None),
            Err(IntegrationError::NoWorkspaces)
        ));
        let ws = Workspace {
            root: dir.path().join("x"),
            kind: WorkspaceKind::GitWorktree { branch: "b".into() },
            base_rev: None,
        };
        assert!(matches!(
            i.integrate(&run, &[ws], IntegrationMode::None),
            Err(IntegrationError::UnsupportedWorkspace { .. })
        ));
    }
}
