//! WS-02 acceptance criterion 4 at the CLI level: `kevin config init | show |
//! validate | rotate-token` (`plan/07-api-and-tui.md` §3, `plan/12` §WS-02).
//! Every invocation runs with `HOME`/`XDG_CONFIG_HOME` pointed at a temp dir
//! so nothing touches the real `~/.config/kevin`.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    cwd: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        Self {
            _dir: dir,
            home,
            cwd,
        }
    }

    fn kevin(&self) -> Command {
        let mut cmd = Command::cargo_bin("kevin").expect("kevin binary is built");
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .current_dir(&self.cwd);
        cmd
    }

    fn config_file(&self) -> PathBuf {
        self.home.join(".config/kevin/kevin.toml")
    }

    fn token_file(&self) -> PathBuf {
        self.home.join(".config/kevin/token")
    }
}

fn stdout(cmd: &mut Command) -> String {
    String::from_utf8(cmd.assert().success().get_output().stdout.clone()).unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn ac_ws02_4_config_init_writes_default_file_and_token_and_refuses_overwrite() {
    let sb = Sandbox::new();
    sb.kevin()
        .args(["config", "init"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kevin.toml"));
    assert_eq!(read(&sb.config_file()), kevin_config::DEFAULT_TOML);
    let token = read(&sb.token_file());
    assert_eq!(token.trim().len(), 43, "32 random bytes, base64url");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(sb.token_file())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
    // The token is never printed.
    let out = stdout(sb.kevin().args(["config", "init", "--force"]));
    assert!(!out.contains(token.trim()));

    // Without --force an existing file is left alone and the command fails.
    std::fs::write(sb.config_file(), "# mine\n").unwrap();
    sb.kevin()
        .args(["config", "init"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
    assert_eq!(read(&sb.config_file()), "# mine\n");

    // --force rewrites the file and the token.
    let before = read(&sb.token_file());
    sb.kevin()
        .args(["config", "init", "--force"])
        .assert()
        .success();
    assert_eq!(read(&sb.config_file()), kevin_config::DEFAULT_TOML);
    assert_ne!(read(&sb.token_file()), before);

    // The written default validates.
    sb.kevin()
        .args(["config", "validate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("configuration is valid"));
}

#[test]
fn ac_ws02_4_config_show_redacts_secrets_and_prints_sources() {
    let sb = Sandbox::new();
    // User layer + project layer + env + --set, with a secret in each place it can live.
    std::fs::create_dir_all(sb.home.join(".config/kevin")).unwrap();
    std::fs::write(
        sb.config_file(),
        "[database]\nurl = \"postgres://kevin:usersecret@db.example/kevin\"\n[kevin]\ninstance_name = \"from-user\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(sb.cwd.join(".kevin")).unwrap();
    std::fs::write(
        sb.cwd.join(".kevin/kevin.toml"),
        "[checks]\ncommands = [\"just ci\"]\n",
    )
    .unwrap();

    let out = stdout(
        sb.kevin()
            .env("KEVIN__SERVER__AUTH_TOKEN_FILE", "/secret/path/token")
            .args([
                "config",
                "show",
                "--sources",
                "--set",
                "kevin.profile=server",
            ]),
    );
    assert!(!out.contains("usersecret"), "{out}");
    assert!(
        out.contains("postgres://kevin:***@db.example/kevin"),
        "{out}"
    );
    assert!(!out.contains("/secret/path/token"), "{out}");
    assert!(out.contains("server.auth_token_file = \"***\""), "{out}");
    assert!(out.contains("# user:"), "{out}");
    assert!(out.contains("# project:"), "{out}");
    assert!(
        out.contains("# env:KEVIN__SERVER__AUTH_TOKEN_FILE"),
        "{out}"
    );
    assert!(out.contains("# --set"), "{out}");
    assert!(out.contains("# profile:server"), "{out}");
    assert!(out.contains("# default"), "{out}");
    assert!(out.contains("kevin.instance_name = \"from-user\""), "{out}");

    // Plain `show` is redacted TOML that parses back into the schema.
    let plain = stdout(sb.kevin().args(["config", "show"]));
    assert!(!plain.contains("usersecret"));
    assert!(plain.starts_with("[kevin]"), "{plain}");
    let _: kevin_config::KevinConfig = toml::from_str(&plain).unwrap();

    // --json carries both config and sources.
    let json: serde_json::Value =
        serde_json::from_str(&stdout(sb.kevin().args(["--json", "config", "show"]))).unwrap();
    assert_eq!(
        json["config"]["database"]["url"],
        "postgres://kevin:***@db.example/kevin"
    );
    assert!(
        json["sources"]["database.url"]
            .as_str()
            .unwrap()
            .starts_with("user:")
    );
    assert!(
        json["sources"]["checks.commands"]
            .as_str()
            .unwrap()
            .starts_with("project:")
    );
}

#[test]
fn ac_ws02_4_config_validate_exits_3_with_every_error() {
    let sb = Sandbox::new();
    let assert = sb
        .kevin()
        .args([
            "config",
            "validate",
            "--set",
            "budget.max_parallel_tasks=0",
            "--set",
            "roles.judge=ghost",
            "--set",
            "workers.codex.sandbox=danger-full-access",
        ])
        .assert()
        .failure()
        .code(3);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("3 configuration errors"), "{stderr}");
    assert!(stderr.contains("budget.max_parallel_tasks"), "{stderr}");
    assert!(stderr.contains("roles.judge"), "{stderr}");
    assert!(stderr.contains("workers.codex.sandbox"), "{stderr}");
    assert!(stderr.contains("--set"), "sources are named: {stderr}");

    // `show` refuses an invalid config too (exit 3).
    sb.kevin()
        .args(["config", "show", "--set", "kevin.profile=moon"])
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("kevin.profile"));

    // --json: machine-readable error list on stdout, still exit 3.
    let assert = sb
        .kevin()
        .args([
            "--json",
            "config",
            "validate",
            "--set",
            "budget.max_attempts=0",
        ])
        .assert()
        .failure()
        .code(3);
    let json: serde_json::Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["errors"][0]["key"], "budget.max_attempts");
}

#[test]
fn ac_ws02_4_config_rotate_token_replaces_the_token_file() {
    let sb = Sandbox::new();
    sb.kevin().args(["config", "init"]).assert().success();
    let before = read(&sb.token_file());
    let out = stdout(sb.kevin().args(["config", "rotate-token"]));
    let after = read(&sb.token_file());
    assert_ne!(before, after);
    assert_eq!(after.trim().len(), 43);
    assert!(out.contains("token"), "{out}");
    assert!(!out.contains(after.trim()), "token is never printed: {out}");

    // Honors server.auth_token_file from the effective config.
    let custom = sb.home.join("custom.token");
    sb.kevin()
        .args([
            "config",
            "rotate-token",
            "--set",
            &format!("server.auth_token_file={}", custom.display()),
        ])
        .assert()
        .success();
    assert_eq!(read(&custom).trim().len(), 43);
}
