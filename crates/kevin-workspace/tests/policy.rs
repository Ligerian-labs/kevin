//! Sandbox policy and env allow-list through the public API (`plan/09-security.md`).

use std::path::PathBuf;

use kevin_domain::{AttemptId, RunId, TaskId, WorkerKind};
use kevin_workspace::{
    EnvAllowlist, EnvAllowlistSpec, FORBIDDEN_FLAGS, KevinEnv, SandboxConfig, SandboxPolicy,
    SandboxTier, check_argv,
};

fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn forbidden_flags_table_matches_plan_09() {
    let rendered: Vec<String> = FORBIDDEN_FLAGS
        .iter()
        .map(|f| format!("{} {}", f.worker, f.render()))
        .collect();
    for expected in [
        "claude --dangerously-skip-permissions",
        "claude --permission-mode bypassPermissions",
        "codex -s danger-full-access",
        "codex --dangerously-bypass-approvals-and-sandbox",
        "codex --dangerously-bypass-hook-trust",
        "opencode --auto",
    ] {
        assert!(
            rendered.iter().any(|r| r == expected),
            "missing {expected} in {rendered:?}"
        );
    }
}

#[test]
fn policy_from_config_follows_the_tier() {
    let native = SandboxPolicy::from(&SandboxConfig::default());
    assert_eq!(native.tier, SandboxTier::CliNative);
    assert!(!native.allow_dangerous_flags);
    let err = native
        .check_argv(
            WorkerKind::Claude,
            &argv("claude -p --output-format stream-json --dangerously-skip-permissions"),
        )
        .unwrap_err();
    assert_eq!(err.flag, "--dangerously-skip-permissions");
    assert_eq!(err.position, 4);
    assert!(
        native
            .check_args(
                WorkerKind::Codex,
                &argv("codex exec --json -s workspace-write -")
            )
            .is_ok()
    );
    assert!(
        native
            .check_argv(WorkerKind::Pi, &argv("pi -p --mode json hello"))
            .is_ok()
    );

    let container = SandboxPolicy::from(&SandboxConfig {
        tier: SandboxTier::Container,
        ..SandboxConfig::default()
    });
    assert!(container.allow_dangerous_flags);
    assert!(
        container
            .check_argv(
                WorkerKind::Codex,
                &argv("codex exec -s danger-full-access -")
            )
            .is_ok()
    );

    // extra_args are part of the final argv and are checked too
    let mut args = argv("opencode run --format json");
    args.extend(argv("--auto hello"));
    assert!(check_argv(WorkerKind::Opencode, &args).is_err());
    assert!(container.check_argv(WorkerKind::Opencode, &args).is_ok());
}

#[test]
fn env_allowlist_is_names_from_config_plus_kevin_vars() {
    let spec = EnvAllowlistSpec::new(["HOME", "PATH", "ANTHROPIC_API_KEY"], ["SSL_CERT_FILE"]);
    let kevin = KevinEnv {
        run_id: RunId::new(),
        task_id: TaskId::new(),
        attempt_id: AttemptId::new(),
        workspace: PathBuf::from("/repo/.kevin/workspaces/r/t-a"),
    };
    let env = EnvAllowlist::build_from(
        &spec,
        &kevin,
        [
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("SSL_CERT_FILE", "/certs"),
            ("OPENAI_API_KEY", "leak"),
            ("DATABASE_URL", "postgres://u:p@h/db"),
        ],
    );
    assert_eq!(env.get("HOME"), Some("/h"));
    assert_eq!(env.get("SSL_CERT_FILE"), Some("/certs"));
    assert_eq!(env.get("OPENAI_API_KEY"), None);
    assert_eq!(env.get("DATABASE_URL"), None);
    assert_eq!(
        env.get("KEVIN_RUN_ID"),
        Some(kevin.run_id.to_string().as_str())
    );
    assert_eq!(
        env.get("KEVIN_TASK_ID"),
        Some(kevin.task_id.to_string().as_str())
    );
    assert_eq!(
        env.get("KEVIN_ATTEMPT_ID"),
        Some(kevin.attempt_id.to_string().as_str())
    );
    assert_eq!(
        env.get("KEVIN_WORKSPACE"),
        Some("/repo/.kevin/workspaces/r/t-a")
    );
    assert_eq!(env.len(), 7);
    // from the real process env: only allow-listed names leak through
    let real = EnvAllowlist::build(
        &EnvAllowlistSpec::new(["PATH"], Vec::<String>::new()),
        &kevin,
    );
    assert!(real.names().all(|n| n == "PATH" || n.starts_with("KEVIN_")));
}
