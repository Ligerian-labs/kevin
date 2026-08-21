//! WS-07 acceptance tests (`plan/12-workstreams.md` §WS-07) on real temp git
//! and jj repositories. jj tests skip with a message when `jj` is missing.

#![allow(clippy::unwrap_used)] // helper fns outside #[test] bodies

mod common;

use common::{RecordingRunner, arg_after, commit_in, git, git_repo, jj, jj_repo, path_of, sh_try};
use kevin_domain::{AttemptId, RunId, TaskId};
use kevin_workspace::{
    ArtifactKind, CleanupOutcome, CleanupPolicy, CmdOutput, IntegrationConfig, IntegrationMode,
    IntegrationRun, Integrator, PrepareRequest, RepoKind, ResolvedStrategy, Workspace,
    WorkspaceConfig, WorkspaceKind, WorkspaceManager, WorkspacePolicy,
};

fn run_cfg() -> WorkspaceConfig {
    WorkspaceConfig::default()
}

// ---------------------------------------------------------------------------
// (1) two attempts on the same repo → disjoint worktrees on distinct branches
// ---------------------------------------------------------------------------

#[test]
fn ac_ws07_1_two_attempts_get_disjoint_worktrees_on_distinct_branches() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    assert_eq!(m.repo_kind(), RepoKind::Git);
    assert_eq!(
        m.resolve_strategy(WorkspacePolicy::Isolated).unwrap(),
        ResolvedStrategy::GitWorktree
    );

    let run = RunId::new();
    let (task_a, task_b) = (TaskId::new(), TaskId::new());
    let a = m
        .prepare_with(
            &PrepareRequest::new(run, task_a, AttemptId::new()).with_slug("Implement API"),
        )
        .unwrap();
    let b = m
        .prepare_with(&PrepareRequest::new(run, task_b, AttemptId::new()).with_slug("Write docs"))
        .unwrap();

    assert_ne!(a.root, b.root, "disjoint directories");
    assert!(a.root.is_dir() && b.root.is_dir());
    assert!(a.root.starts_with(root.join(".kevin/workspaces")));
    assert!(
        a.root
            .parent()
            .unwrap()
            .ends_with(m.run_dir(run).file_name().unwrap())
    );
    let (ba, bb) = (
        a.branch().unwrap().to_owned(),
        b.branch().unwrap().to_owned(),
    );
    assert_ne!(ba, bb, "distinct branches");
    assert!(
        ba.starts_with("kevin/") && ba.ends_with("/implement-api"),
        "{ba}"
    );
    assert!(bb.ends_with("/write-docs"), "{bb}");
    assert_eq!(
        a.base_rev.as_deref(),
        Some(git(&root, &["rev-parse", "HEAD"]).trim())
    );

    // Both are registered worktrees, each checked out on its own branch.
    let list = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains(&format!("worktree {}", a.root.display())),
        "{list}"
    );
    assert!(
        list.contains(&format!("worktree {}", b.root.display())),
        "{list}"
    );
    assert_eq!(
        git(&a.root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        ba
    );
    assert_eq!(
        git(&b.root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        bb
    );

    // Writes are isolated: a commit in A is invisible in B and at the root.
    commit_in(&a.root, "a.txt", "from a\n", "a");
    assert!(!b.root.join("a.txt").exists());
    assert!(!root.join("a.txt").exists());

    // A retry of task A gets a fresh, distinct worktree and branch.
    let a2 = m
        .prepare_with(
            &PrepareRequest::new(run, task_a, AttemptId::new()).with_slug("Implement API"),
        )
        .unwrap();
    assert_ne!(a2.root, a.root);
    assert_ne!(a2.branch(), a.branch());
    assert!(
        a2.branch().unwrap().starts_with(&ba),
        "retry branch derives from the task branch"
    );
    assert!(
        !a2.root.join("a.txt").exists(),
        "retry starts from HEAD, not from the failed attempt"
    );

    // cleanup: on_success removes the worktree but keeps the branch for integration.
    assert_eq!(
        m.cleanup(&a, CleanupPolicy::OnSuccess, true).unwrap(),
        CleanupOutcome::Removed
    );
    assert!(!a.root.exists());
    assert!(git(&root, &["branch", "--list", &ba]).contains(&ba));
    assert_eq!(
        m.cleanup(&b, CleanupPolicy::OnSuccess, false).unwrap(),
        CleanupOutcome::Kept
    );
    assert!(b.root.exists());
    assert_eq!(
        m.cleanup(&b, CleanupPolicy::Always, false).unwrap(),
        CleanupOutcome::Removed
    );
    m.discard_branch(&a).unwrap();
    assert!(!git(&root, &["branch", "--list", &ba]).contains(&ba));
}

// ---------------------------------------------------------------------------
// (2) jj detection when `.jj` exists (colocated counts as jj)
// ---------------------------------------------------------------------------

#[test]
fn ac_ws07_2_jj_detected_when_dot_jj_exists() {
    let Some(repo) = jj_repo(true) else { return };
    let root = path_of(&repo);
    assert!(root.join(".git").exists(), "fixture is colocated");
    assert_eq!(
        RepoKind::detect(&root),
        RepoKind::Jj,
        "colocated repo is jj"
    );
    assert_eq!(RepoKind::locate(root.join("nope")).unwrap().0, RepoKind::Jj);

    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    assert_eq!(m.repo_kind(), RepoKind::Jj);
    assert_eq!(
        m.resolve_strategy(WorkspacePolicy::Isolated).unwrap(),
        ResolvedStrategy::JjWorkspace
    );

    let run = RunId::new();
    let ws = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("impl"))
        .unwrap();
    let WorkspaceKind::JjWorkspace { name } = &ws.kind else {
        panic!("expected a jj workspace, got {:?}", ws.kind);
    };
    assert!(ws.root.join(".jj").exists());
    assert!(jj(&root, &["workspace", "list"]).contains(&format!("{name}:")));
    assert!(
        ws.base_rev.as_ref().is_some_and(|r| r.len() == 40),
        "{:?}",
        ws.base_rev
    );

    // A second attempt: another workspace, another name.
    let ws2 = m.prepare(run, TaskId::new(), AttemptId::new()).unwrap();
    assert_ne!(ws2.root, ws.root);
    assert_ne!(ws2.jj_name(), ws.jj_name());

    // Cleanup forgets the workspace and leaves a bookmark naming the result.
    commit_in(&ws.root, "x.txt", "x\n", "x");
    assert_eq!(
        m.cleanup(&ws, CleanupPolicy::Always, true).unwrap(),
        CleanupOutcome::Removed
    );
    assert!(!ws.root.exists());
    assert!(!jj(&root, &["workspace", "list"]).contains(&format!("{name}:")));
    let bookmarks = jj(&root, &["bookmark", "list"]);
    assert!(bookmarks.contains(&m.jj_bookmark(name)), "{bookmarks}");

    // Non-colocated repos are jj as well; plain git is git.
    if let Some(plain) = jj_repo(false) {
        assert!(!plain.path().join(".git").exists());
        assert_eq!(RepoKind::detect(plain.path()), RepoKind::Jj);
    }
    assert_eq!(RepoKind::detect(git_repo().path()), RepoKind::Git);
}

// ---------------------------------------------------------------------------
// (3) conflict between two branches is reported, never silently resolved
// ---------------------------------------------------------------------------

fn two_conflicting_attempts(
    root: &std::path::Path,
    m: &WorkspaceManager,
    run: RunId,
) -> Vec<Workspace> {
    let a = m
        .prepare_with(
            &PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("task a"),
        )
        .unwrap();
    let b = m
        .prepare_with(
            &PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("task b"),
        )
        .unwrap();
    commit_in(
        &a.root,
        "shared.txt",
        "from A\nline2\nline3\n",
        "A edits shared",
    );
    commit_in(
        &b.root,
        "shared.txt",
        "from B\nline2\nline3\n",
        "B edits shared",
    );
    assert_eq!(
        std::fs::read_to_string(root.join("shared.txt")).unwrap(),
        "line1\nline2\nline3\n"
    );
    vec![a, b]
}

#[test]
fn ac_ws07_3_merge_conflict_is_reported_not_resolved() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let wss = two_conflicting_attempts(&root, &m, run);
    let main_before = git(&root, &["rev-parse", "main"]);

    let integrator = Integrator::new(&root, IntegrationConfig::default()).unwrap();
    let result = integrator
        .integrate(
            &IntegrationRun::new(run, "Conflicting run"),
            &wss,
            IntegrationMode::Merge,
        )
        .unwrap();

    assert!(!result.is_clean());
    assert_eq!(result.conflicts.len(), 1, "{:?}", result.conflicts);
    let c = &result.conflicts[0];
    assert_eq!(
        c.source,
        wss[1].branch().unwrap(),
        "second branch conflicts with the first"
    );
    assert_eq!(c.files, vec!["shared.txt".to_owned()]);
    assert!(c.detail.contains("CONFLICT"), "{}", c.detail);
    // Nothing was resolved or merged into main; the integration worktree is left for an Integrate task.
    assert_eq!(git(&root, &["rev-parse", "main"]), main_before);
    assert_eq!(
        std::fs::read_to_string(root.join("shared.txt")).unwrap(),
        "line1\nline2\nline3\n"
    );
    let iws = result
        .integration_workspace
        .as_ref()
        .expect("integration workspace kept");
    assert!(iws.root.is_dir());
    assert_eq!(
        git(&iws.root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        iws.branch().unwrap()
    );
    assert!(
        !std::fs::read_to_string(iws.root.join("shared.txt"))
            .unwrap()
            .contains("<<<<<<<"),
        "merge aborted, no markers left"
    );
    assert!(result.pr_urls().next().is_none());
}

#[test]
fn merge_mode_merges_clean_branches_into_base() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let a = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("a"))
        .unwrap();
    let b = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("b"))
        .unwrap();
    commit_in(&a.root, "a.txt", "a\n", "a");
    commit_in(&b.root, "b.txt", "b\n", "b");

    let artifacts = tempfile::tempdir().unwrap();
    let cfg = IntegrationConfig {
        artifacts_dir: Some(artifacts.path().to_path_buf()),
        checks: vec!["test -f a.txt && test -f b.txt".to_owned()],
        ..IntegrationConfig::default()
    };
    let integrator = Integrator::new(&root, cfg).unwrap();
    let result = integrator
        .integrate(
            &IntegrationRun::new(run, "Clean run"),
            &[a.clone(), b.clone()],
            IntegrationMode::Merge,
        )
        .unwrap();
    assert!(result.is_clean(), "{:?}", result.conflicts);
    assert_eq!(result.integration_branch.as_deref(), Some("main"));
    assert!(result.integration_workspace.is_none());
    // main (checked out at the root) now contains both files.
    assert!(root.join("a.txt").exists() && root.join("b.txt").exists());
    assert_eq!(
        git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "main"
    );
    let diff = result
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Diff)
        .expect("diff artifact");
    assert!(diff.uri.starts_with("file://"));
    assert!(diff.sha256.as_ref().is_some_and(|s| s.len() == 64));
    let path = diff.uri.trim_start_matches("file://");
    let text = std::fs::read_to_string(path).unwrap();
    assert!(
        text.contains("+++ b/a.txt") && text.contains("+++ b/b.txt"),
        "{text}"
    );
    assert_eq!(diff.bytes, Some(text.len() as u64));
    // integration worktree and branch are gone
    let list = git(&root, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains("integration"), "{list}");
    assert!(!git(&root, &["branch", "--list", "kevin/*/integration"]).contains("integration"));
}

#[test]
fn failing_checks_block_integration() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let a = m.prepare(run, TaskId::new(), AttemptId::new()).unwrap();
    commit_in(&a.root, "a.txt", "a\n", "a");
    let cfg = IntegrationConfig {
        checks: vec!["exit 7".to_owned()],
        ..IntegrationConfig::default()
    };
    let err = Integrator::new(&root, cfg)
        .unwrap()
        .integrate(&IntegrationRun::new(run, "x"), &[a], IntegrationMode::Merge)
        .unwrap_err();
    assert!(
        matches!(
            err,
            kevin_workspace::IntegrationError::ChecksFailed { code: 7, .. }
        ),
        "{err}"
    );
    assert!(
        !root.join("a.txt").exists(),
        "base untouched when checks fail"
    );
}

#[test]
fn jj_merge_conflict_is_reported_not_resolved() {
    let Some(repo) = jj_repo(true) else { return };
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let wss = two_conflicting_attempts(&root, &m, run);
    let main_before = jj(
        &root,
        &["log", "--no-graph", "-r", "main", "-T", "commit_id"],
    );

    let integrator = Integrator::new(&root, IntegrationConfig::default()).unwrap();
    let result = integrator
        .integrate(
            &IntegrationRun::new(run, "Conflicting jj run"),
            &wss,
            IntegrationMode::Merge,
        )
        .unwrap();
    assert_eq!(result.conflicts.len(), 1, "{:?}", result.conflicts);
    assert_eq!(result.conflicts[0].source, wss[1].jj_name().unwrap());
    assert_eq!(result.conflicts[0].files, vec!["shared.txt".to_owned()]);
    assert_eq!(
        jj(
            &root,
            &["log", "--no-graph", "-r", "main", "-T", "commit_id"]
        ),
        main_before
    );
    let iws = result
        .integration_workspace
        .expect("integration workspace kept");
    assert!(iws.root.join(".jj").exists());
    assert_eq!(
        jj(
            &iws.root,
            &["log", "--no-graph", "-r", "@", "-T", "conflict"]
        )
        .trim(),
        "false",
        "integration working copy parked on the last clean state"
    );
}

#[test]
fn jj_merge_mode_moves_base_bookmark() {
    let Some(repo) = jj_repo(true) else { return };
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let a = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("a"))
        .unwrap();
    let b = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("b"))
        .unwrap();
    commit_in(&a.root, "a.txt", "a\n", "a");
    commit_in(&b.root, "b.txt", "b\n", "b");
    // `a` was cleaned up (default policy) before integration: only its bookmark remains.
    m.cleanup(&a, CleanupPolicy::OnSuccess, true).unwrap();

    let result = Integrator::new(&root, IntegrationConfig::default())
        .unwrap()
        .integrate(
            &IntegrationRun::new(run, "jj run"),
            &[a, b],
            IntegrationMode::Merge,
        )
        .unwrap();
    assert!(result.is_clean(), "{:?}", result.conflicts);
    assert_eq!(result.integration_branch.as_deref(), Some("main"));
    let files = jj(&root, &["file", "list", "-r", "main"]);
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files}"
    );
    assert!(!jj(&root, &["workspace", "list"]).contains("integration"));
}

// ---------------------------------------------------------------------------
// (4) `.kevin/workspaces` is excluded via `.git/info/exclude` (jj: same file of its git store)
// ---------------------------------------------------------------------------

#[test]
fn ac_ws07_4_workspaces_dir_is_added_to_git_info_exclude() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let exclude = root.join(".git/info/exclude");
    let before = std::fs::read_to_string(&exclude).unwrap_or_default();
    assert!(!before.contains(".kevin/workspaces"));

    let ws = m
        .prepare(RunId::new(), TaskId::new(), AttemptId::new())
        .unwrap();
    let after = std::fs::read_to_string(&exclude).unwrap();
    assert!(after.lines().any(|l| l == "/.kevin/workspaces/"), "{after}");
    assert!(
        !root.join(".gitignore").exists(),
        "never touches .gitignore"
    );
    assert_eq!(
        git(&root, &["status", "--porcelain"]).trim(),
        "",
        "workspace dir invisible to git"
    );
    assert_eq!(
        git(&ws.root, &["status", "--porcelain"]).trim(),
        "",
        "and inside the worktree"
    );

    // Idempotent: a second manager / prepare does not duplicate the line.
    let m2 = WorkspaceManager::new(&root, run_cfg()).unwrap();
    m2.prepare(RunId::new(), TaskId::new(), AttemptId::new())
        .unwrap();
    m2.ensure_excluded().unwrap();
    let again = std::fs::read_to_string(&exclude).unwrap();
    assert_eq!(again.matches("/.kevin/workspaces/").count(), 1, "{again}");

    // jj: colocated → `.git/info/exclude`; non-colocated → the store's git dir. Either way `jj status` is clean.
    for colocate in [true, false] {
        let Some(jrepo) = jj_repo(colocate) else {
            break;
        };
        let jroot = path_of(&jrepo);
        let jm = WorkspaceManager::new(&jroot, run_cfg()).unwrap();
        let file = jm.exclude_file().expect("jj repos have a git exclude file");
        if colocate {
            assert_eq!(file, jroot.join(".git/info/exclude"));
        } else {
            assert!(file.starts_with(jroot.join(".jj")), "{}", file.display());
        }
        jm.prepare(RunId::new(), TaskId::new(), AttemptId::new())
            .unwrap();
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("/.kevin/workspaces/")
        );
        let status = jj(&jroot, &["status"]);
        assert!(
            status.contains("The working copy has no changes"),
            "{status}"
        );
    }
}

// ---------------------------------------------------------------------------
// (5) integration = pr → `gh pr create` (mocked) with the acceptance criteria in the body
// ---------------------------------------------------------------------------

#[test]
fn ac_ws07_5_pr_mode_invokes_gh_pr_create_with_acceptance_criteria() {
    let repo = git_repo();
    let root = path_of(&repo);
    let runner = RecordingRunner::new().stub_remote("https://github.com/acme/app/pull/42");
    let m = WorkspaceManager::with_runner(&root, run_cfg(), runner.clone()).unwrap();
    let run = RunId::new();
    let a = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("api"))
        .unwrap();
    let b = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("docs"))
        .unwrap();
    commit_in(&a.root, "api.rs", "fn api() {}\n", "api");
    commit_in(&b.root, "docs.md", "# docs\n", "docs");

    let integrator =
        Integrator::with_runner(&root, IntegrationConfig::default(), runner.clone()).unwrap();
    let run_info = IntegrationRun {
        run_id: run,
        title: "Add the API and its docs".into(),
        summary: "Implements the HTTP API and documents it.".into(),
        acceptance_criteria: vec![
            "GET /health returns 200".into(),
            "docs/api.md describes every endpoint".into(),
        ],
        base_branch: None,
    };
    let result = integrator
        .integrate(&run_info, &[a.clone(), b.clone()], IntegrationMode::Pr)
        .unwrap();

    assert!(result.is_clean());
    let ibranch = integrator.integration_branch(run);
    assert_eq!(result.integration_branch.as_deref(), Some(ibranch.as_str()));
    assert_eq!(
        result.pr_urls().collect::<Vec<_>>(),
        ["https://github.com/acme/app/pull/42"]
    );

    let pushes = runner.calls_of("git", &["push"]);
    assert_eq!(pushes.len(), 1, "{pushes:?}");
    assert_eq!(pushes[0].args, ["push", "-u", "origin", ibranch.as_str()]);

    let gh = runner.calls_of("gh", &["pr", "create"]);
    assert_eq!(gh.len(), 1, "exactly one `gh pr create`: {gh:?}");
    let args = &gh[0].args;
    assert_eq!(arg_after(args, "--base"), Some("main"));
    assert_eq!(arg_after(args, "--head"), Some(ibranch.as_str()));
    assert_eq!(arg_after(args, "--title"), Some("Add the API and its docs"));
    let body = arg_after(args, "--body").expect("--body");
    assert!(body.contains("## Acceptance criteria"), "{body}");
    assert!(body.contains("- [ ] GET /health returns 200"), "{body}");
    assert!(
        body.contains("- [ ] docs/api.md describes every endpoint"),
        "{body}"
    );
    assert!(body.contains("Implements the HTTP API and documents it."));
    assert!(body.contains(a.branch().unwrap()) && body.contains(b.branch().unwrap()));
    assert_eq!(gh[0].cwd.as_deref(), Some(root.as_path()));

    // The integration branch exists locally with both task branches merged; worktree removed.
    let files = git(&root, &["ls-tree", "--name-only", &ibranch]);
    assert!(
        files.contains("api.rs") && files.contains("docs.md"),
        "{files}"
    );
    assert!(!git(&root, &["worktree", "list", "--porcelain"]).contains("integration"));
    assert!(!root.join("api.rs").exists(), "main untouched in pr mode");
}

#[test]
fn pr_per_task_opens_one_pr_per_branch() {
    let repo = git_repo();
    let root = path_of(&repo);
    let runner = RecordingRunner::new();
    runner.stub("git", &["push"], CmdOutput::ok());
    runner.stub(
        "gh",
        &[],
        CmdOutput::ok_with("https://github.com/acme/app/pull/7\n"),
    );
    let m = WorkspaceManager::with_runner(&root, run_cfg(), runner.clone()).unwrap();
    let run = RunId::new();
    let a = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("a"))
        .unwrap();
    let b = m
        .prepare_with(&PrepareRequest::new(run, TaskId::new(), AttemptId::new()).with_slug("b"))
        .unwrap();
    commit_in(&a.root, "a.txt", "a\n", "a");
    commit_in(&b.root, "b.txt", "b\n", "b");
    let cfg = IntegrationConfig {
        pr_per_task: true,
        ..IntegrationConfig::default()
    };
    let mut info = IntegrationRun::new(run, "Per task");
    info.acceptance_criteria = vec!["criterion one".into()];
    let result = Integrator::with_runner(&root, cfg, runner.clone())
        .unwrap()
        .integrate(&info, &[a.clone(), b.clone()], IntegrationMode::Pr)
        .unwrap();
    assert_eq!(result.pr_urls().count(), 2);
    let gh = runner.calls_of("gh", &["pr", "create"]);
    assert_eq!(gh.len(), 2);
    let heads: Vec<&str> = gh
        .iter()
        .filter_map(|c| arg_after(&c.args, "--head"))
        .collect();
    assert_eq!(heads, [a.branch().unwrap(), b.branch().unwrap()]);
    assert!(gh.iter().all(|c| {
        arg_after(&c.args, "--body")
            .unwrap()
            .contains("- [ ] criterion one")
    }));
    assert_eq!(runner.calls_of("git", &["push"]).len(), 2);
}

#[test]
fn integration_none_leaves_branches_as_artifacts() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let a = m.prepare(run, TaskId::new(), AttemptId::new()).unwrap();
    let runner = RecordingRunner::new();
    let result = Integrator::with_runner(&root, IntegrationConfig::default(), runner.clone())
        .unwrap()
        .integrate(
            &IntegrationRun::new(run, "none"),
            std::slice::from_ref(&a),
            IntegrationMode::None,
        )
        .unwrap();
    assert_eq!(result.artifacts.len(), 1);
    assert_eq!(result.artifacts[0].kind, ArtifactKind::Report);
    assert_eq!(
        result.artifacts[0].uri,
        format!("branch:{}", a.branch().unwrap())
    );
    assert!(runner.calls_of("gh", &[]).is_empty());
    assert!(runner.calls_of("git", &["push"]).is_empty());
    assert!(
        sh_try(
            &root,
            "git",
            &["rev-parse", "--verify", a.branch().unwrap()]
        )
        .0
    );
}

// ---------------------------------------------------------------------------
// policies: shared and read-only tasks
// ---------------------------------------------------------------------------

#[test]
fn shared_policy_reuses_one_workspace_and_read_only_runs_in_place() {
    let repo = git_repo();
    let root = path_of(&repo);
    let m = WorkspaceManager::new(&root, run_cfg()).unwrap();
    let run = RunId::new();
    let s1 = m
        .prepare_with(
            &PrepareRequest::new(run, TaskId::new(), AttemptId::new())
                .with_policy(WorkspacePolicy::Shared),
        )
        .unwrap();
    let s2 = m
        .prepare_with(
            &PrepareRequest::new(run, TaskId::new(), AttemptId::new())
                .with_policy(WorkspacePolicy::Shared),
        )
        .unwrap();
    assert_eq!(s1, s2, "shared tasks of a run share one workspace");
    assert!(s1.root.ends_with("shared"));
    assert!(s1.branch().unwrap().ends_with("/shared"));

    let ro = m
        .prepare_with(
            &PrepareRequest::new(run, TaskId::new(), AttemptId::new())
                .with_policy(WorkspacePolicy::ReadOnly),
        )
        .unwrap();
    assert!(ro.is_in_place());
    assert_eq!(ro.root, root);
    assert_eq!(
        ro.base_rev.as_deref(),
        Some(git(&root, &["rev-parse", "HEAD"]).trim())
    );
    assert!(
        m.in_place_holder().is_none(),
        "read-only never takes the write lease"
    );

    assert_eq!(m.list_run(run).unwrap(), vec![s1.clone()]);
    m.cleanup_run(run).unwrap();
    assert!(!m.run_dir(run).exists());
}
