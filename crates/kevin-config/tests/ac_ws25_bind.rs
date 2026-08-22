//! WS-25 security checklist — `plan/09-security.md` §API authentication:
//! "Binding a non-loopback address requires `server.auth_token_file` to exist
//! with mode `0600` (checked at startup; otherwise `ConfigError::InsecureBind`)".
//!
//! Validation only checked that a *path was configured*, so a non-loopback
//! bind was accepted with a token file that did not exist or was
//! world-readable — the worst case, because the operator believes the port is
//! protected. [`check_bind_security`] is the startup check `kevin serve` runs
//! before it binds.
//!
//! It is deliberately *not* part of `load()`: loading must stay a pure
//! function of the configuration layers (a config is often validated on a
//! machine that will never serve it, and `kevin config init` writes the token
//! after the first `kevin config show`).

#![cfg(unix)]

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use kevin_config::token::check_bind_security;
use kevin_config::{ConfigError, KevinConfig, Profile};

/// Writes a token file with `mode`.
fn token_file(dir: &Path, name: &str, contents: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

fn config(bind: &str, token: Option<PathBuf>) -> KevinConfig {
    let mut config = KevinConfig::default();
    config.server.bind = bind.parse::<SocketAddr>().expect("bind");
    config.server.auth_token_file = token.unwrap_or_default();
    config
}

fn reason(err: ConfigError) -> String {
    match err {
        ConfigError::InsecureBind { reason, .. } => reason,
        other => panic!("expected InsecureBind, got {other:?}"),
    }
}

#[test]
fn ac_ws25_13_1_a_loopback_bind_never_needs_a_token_file() {
    // Not even a configured one, and the filesystem is never touched.
    assert!(check_bind_security(&config("127.0.0.1:7777", None)).is_ok());
    assert!(check_bind_security(&config("[::1]:7777", None)).is_ok());
    assert!(
        check_bind_security(&config(
            "127.0.0.1:7777",
            Some(PathBuf::from("/does/not/exist"))
        ))
        .is_ok()
    );
}

#[test]
fn ac_ws25_13_2_a_public_bind_needs_a_token_file_that_exists_with_mode_0600() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Nothing configured.
    let err = check_bind_security(&config("0.0.0.0:7777", None)).expect_err("no token");
    assert!(reason(err).contains("no token file"));

    // Configured but absent — the case validation used to accept.
    let missing = dir.path().join("absent");
    let err = check_bind_security(&config("0.0.0.0:7777", Some(missing.clone())))
        .expect_err("missing token");
    let why = reason(err);
    assert!(why.contains("does not exist"), "{why}");
    assert!(why.contains(&missing.display().to_string()), "{why}");

    // Present but world-readable.
    let loose = token_file(dir.path(), "loose", "s3cret-token-value\n", 0o644);
    let err =
        check_bind_security(&config("0.0.0.0:7777", Some(loose))).expect_err("loose permissions");
    let why = reason(err);
    assert!(why.contains("0644"), "{why}");
    assert!(why.contains("expected 0600"), "{why}");

    // Present, 0600, but empty: an empty token protects nothing.
    let empty = token_file(dir.path(), "empty", "", 0o600);
    let err = check_bind_security(&config("0.0.0.0:7777", Some(empty))).expect_err("empty token");
    assert!(reason(err).contains("is empty"));

    // The one shape that is allowed.
    let good = token_file(dir.path(), "good", "s3cret-token-value\n", 0o600);
    assert!(check_bind_security(&config("0.0.0.0:7777", Some(good))).is_ok());
}

#[test]
fn ac_ws25_13_3_the_kohral_token_can_protect_the_bind_and_is_checked_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = config("0.0.0.0:7777", None);
    config.kevin.profile = Profile::Kohral;

    // A Kohral runtime token that does not exist is reported as such rather
    // than passing because a path was set.
    config.kohral.token_file = dir.path().join("absent-kohral");
    let err = check_bind_security(&config).expect_err("missing kohral token");
    assert!(reason(err).contains("does not exist"));

    // With a real 0600 file the bind is protected even without an API token.
    config.kohral.token_file = token_file(dir.path(), "kohral", "kohral-runtime-token\n", 0o600);
    assert!(check_bind_security(&config).is_ok());

    // Outside the Kohral profile the Kohral token is irrelevant: the operator
    // API port is a different surface with a different credential
    // (`plan/09` §Kohral boundary).
    let mut plain = config.clone();
    plain.kevin.profile = Profile::Laptop;
    plain.kohral.enabled = false;
    let err = check_bind_security(&plain).expect_err("kohral token does not cover the API port");
    assert!(reason(err).contains("no token file"));
}
