//! WS-21 acceptance criteria (`plan/12-workstreams.md` §WS-21): release
//! engineering. What a test can check from here is the *shape* of the release
//! artefacts that live in the repository — the binary's version/provenance
//! string, the release workflow, the container image definition and the
//! changelog. Building the image and cutting a tag are exercised by
//! `podman build -f deploy/Dockerfile .` and by CI on a `v*` tag; both are
//! deliberately kept out of `just ci` (see `docs/releasing.md`).

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn kevin() -> Command {
    Command::cargo_bin("kevin").expect("kevin binary is built")
}

/// `kevin --version` prints `kevin <semver> (<sha> <YYYY-MM-DD>)`.
///
/// The sha is the abbreviated commit id, or the literal `unknown` when the
/// source tree has no git metadata (source tarball / vendored build) — both
/// are accepted, the point is that the shape never changes.
#[test]
fn ac_ws21_1_version_prints_semver_git_sha_and_build_date() {
    let out = kevin()
        .arg("--version")
        .output()
        .expect("run kevin --version");
    assert!(
        out.status.success(),
        "kevin --version exited with {:?}",
        out.status
    );
    let stdout = String::from_utf8(out.stdout).expect("utf-8 version output");
    let line = stdout.trim();

    let rest = line
        .strip_prefix("kevin ")
        .unwrap_or_else(|| panic!("version line {line:?} must start with `kevin `"));
    let (semver, provenance) = rest
        .split_once(" (")
        .unwrap_or_else(|| panic!("version line {line:?} must be `kevin <semver> (<sha> <date>)`"));
    let provenance = provenance
        .strip_suffix(')')
        .unwrap_or_else(|| panic!("version line {line:?} must end with `)`"));

    // semver: MAJOR.MINOR.PATCH (with an optional pre-release/build suffix).
    let core = semver.split(['-', '+']).next().unwrap_or_default();
    let numbers: Vec<&str> = core.split('.').collect();
    assert_eq!(numbers.len(), 3, "{semver:?} is not a semver triple");
    assert!(
        numbers
            .iter()
            .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())),
        "{semver:?} is not numeric"
    );
    assert_eq!(
        semver,
        env!("CARGO_PKG_VERSION"),
        "version must match the workspace package version"
    );

    let (sha, date) = provenance
        .split_once(' ')
        .unwrap_or_else(|| panic!("{provenance:?} must be `<sha> <date>`"));
    assert!(
        sha == "unknown" || (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())),
        "{sha:?} is neither an abbreviated commit id nor `unknown`"
    );
    let date_parts: Vec<&str> = date.split('-').collect();
    assert_eq!(date_parts.len(), 3, "{date:?} is not YYYY-MM-DD");
    assert_eq!(
        (
            date_parts[0].len(),
            date_parts[1].len(),
            date_parts[2].len()
        ),
        (4, 2, 2),
        "{date:?} is not YYYY-MM-DD"
    );
    assert!(
        date_parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_digit())),
        "{date:?} is not numeric"
    );
}

/// `kevin -V` and `kevin <subcommand> --version` report the same build
/// (clap `propagate_version`), so a bug report from any command identifies the
/// binary.
#[test]
fn ac_ws21_2_version_is_propagated_to_subcommands() {
    let short = kevin().arg("-V").output().expect("kevin -V");
    let long = kevin().arg("--version").output().expect("kevin --version");
    assert_eq!(short.stdout, long.stdout, "-V and --version must agree");

    let sub = kevin()
        .args(["serve", "--version"])
        .output()
        .expect("kevin serve --version");
    let sub_out = String::from_utf8(sub.stdout).expect("utf-8");
    let expected = String::from_utf8(long.stdout).expect("utf-8");
    let (_, want) = expected.trim().split_once(' ').expect("`kevin <version>`");
    assert!(
        sub_out.contains(want),
        "subcommand version {sub_out:?} must contain {want:?}"
    );
}

/// A tagged build produces binaries for the four supported targets plus a
/// multi-arch image with SBOM and provenance, and the release is created with
/// checksums.
#[test]
fn ac_ws21_3_release_workflow_covers_targets_checksums_and_image() {
    let workflow = read(".github/workflows/release.yml");

    for target in [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
    ] {
        assert!(workflow.contains(target), "release.yml must build {target}");
    }
    for marker in [
        "SHA256SUMS",        // checksums attached to the release
        "tar.gz",            // archive format
        "linux/amd64",       // multi-arch image
        "linux/arm64",       //
        "ghcr.io/",          // published by digest to GHCR
        "cosign",            // keyless signature
        "sbom",              // SBOM attached to the image
        "provenance",        // SLSA provenance attestation
        "workflow_dispatch", // manual/dry runs
    ] {
        assert!(
            workflow.contains(marker),
            "release.yml must mention {marker}"
        );
    }
    assert!(
        workflow.contains("tags:"),
        "release.yml must trigger on tags"
    );
    assert!(
        workflow.contains("v*"),
        "release.yml must trigger on `v*` tags"
    );
}

/// The container image is the plain Kevin daemon: it runs `kevin serve`, does
/// not bundle the agent CLIs (that is WS-23's Kohral image) and does not run
/// as root.
#[test]
fn ac_ws21_4_container_image_runs_kevin_serve_unprivileged() {
    let dockerfile = read("deploy/Dockerfile");
    assert!(
        dockerfile.contains("rust:1-trixie AS builder"),
        "builder stage must be the rust image"
    );
    assert!(
        dockerfile.contains("debian:trixie-slim"),
        "runtime stage must be a slim base"
    );
    assert!(
        dockerfile.contains("USER kevin"),
        "the runtime stage must drop root"
    );
    assert!(
        dockerfile.contains(r#"CMD ["serve"]"#),
        "the image must default to `kevin serve`"
    );
    assert!(dockerfile.contains("7777"), "the API port must be exposed");
    for cli in ["claude", "codex", "opencode"] {
        assert!(
            !dockerfile.contains(&format!("install {cli}")),
            "the daemon image must not bundle the {cli} CLI"
        );
    }

    let readme = read("deploy/README.md");
    for topic in ["KEVIN__DATABASE__URL", "data_dir", "7777", "laptop", "VPS"] {
        assert!(
            readme.contains(topic),
            "deploy/README.md must document {topic}"
        );
    }
}

/// Keep a Changelog + semver, with an `Unreleased` section that release
/// tagging turns into `[X.Y.Z]`, and a runbook that says how.
#[test]
fn ac_ws21_5_changelog_and_release_runbook_exist() {
    let changelog = read("CHANGELOG.md");
    assert!(
        changelog.contains("Keep a Changelog"),
        "CHANGELOG.md must follow Keep a Changelog"
    );
    assert!(
        changelog.contains("## [Unreleased]"),
        "CHANGELOG.md must have an Unreleased section"
    );

    let runbook = read("docs/releasing.md");
    for topic in ["vX.Y.Z", "cosign", "rollback", "cargo install"] {
        assert!(
            runbook.contains(topic),
            "docs/releasing.md must cover {topic}"
        );
    }

    let readme = read("README.md");
    assert!(
        readme.contains("cargo install --path crates/kevin-cli"),
        "README must document the source install"
    );
}
