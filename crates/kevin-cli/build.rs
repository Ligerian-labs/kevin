//! Build script for the `kevin` binary: stamps provenance into the executable
//! so `kevin --version` (and the `kevin_build_info` metric described in
//! `plan/10-observability-ops.md`) can report *which* build is running.
//!
//! Two variables are emitted, both always defined so `env!` in the crate never
//! fails to compile:
//!
//! - `KEVIN_BUILD_GIT_SHA` — abbreviated commit id, or `unknown` when the
//!   source has no git metadata (release source tarball, vendored build,
//!   `cargo install` from a `.crate` file).
//! - `KEVIN_BUILD_DATE` — build date as `YYYY-MM-DD` (UTC).
//!
//! Both can be overridden from the environment, which is what packagers and
//! reproducible builds need: CI passes `KEVIN_BUILD_GIT_SHA` when it builds
//! from a tarball, and `SOURCE_DATE_EPOCH` (the reproducible-builds standard)
//! pins the date. No dependencies on purpose — this script must not add
//! anything to the build graph that `cargo deny` has to vet.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Value used when the commit id cannot be determined.
const UNKNOWN_SHA: &str = "unknown";
/// Length of the abbreviated commit id.
const SHA_LEN: usize = 9;

fn main() {
    // Re-run when the override variables change, and when HEAD moves.
    println!("cargo:rerun-if-env-changed=KEVIN_BUILD_GIT_SHA");
    println!("cargo:rerun-if-env-changed=KEVIN_BUILD_DATE");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    println!("cargo:rustc-env=KEVIN_BUILD_GIT_SHA={}", git_sha());
    println!("cargo:rustc-env=KEVIN_BUILD_DATE={}", build_date());
}

/// Abbreviated commit id: `$KEVIN_BUILD_GIT_SHA`, else `git rev-parse`, else
/// [`UNKNOWN_SHA`]. Never fails the build — provenance is nice to have, a
/// broken build is not.
fn git_sha() -> String {
    if let Some(sha) = non_empty_env("KEVIN_BUILD_GIT_SHA") {
        return sanitize(&sha);
    }
    let output = Command::new("git")
        .args(["rev-parse", &format!("--short={SHA_LEN}"), "HEAD"])
        .current_dir(manifest_dir())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            let sha = sanitize(&String::from_utf8_lossy(&out.stdout));
            if sha.is_empty() {
                UNKNOWN_SHA.to_owned()
            } else {
                sha
            }
        }
        // `git` missing, not a repository, or a shallow/empty checkout.
        _ => UNKNOWN_SHA.to_owned(),
    }
}

/// Build date as `YYYY-MM-DD` (UTC): `$KEVIN_BUILD_DATE`, else
/// `$SOURCE_DATE_EPOCH`, else the wall clock.
fn build_date() -> String {
    if let Some(date) = non_empty_env("KEVIN_BUILD_DATE") {
        return sanitize(&date);
    }
    let secs = non_empty_env("SOURCE_DATE_EPOCH")
        .and_then(|v| v.parse::<i64>().ok())
        .or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_secs()).ok())
        })
        .unwrap_or_default();
    let (year, month, day) = civil_from_unix_seconds(secs);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Files whose change should invalidate the stamp: `HEAD` plus the ref it
/// points at (so a commit on the current branch is picked up).
fn git_watch_paths() -> Vec<PathBuf> {
    let Some(git_dir) = git_dir() else {
        return Vec::new();
    };
    let head = git_dir.join("HEAD");
    let mut paths = Vec::new();
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(reference) = contents.strip_prefix("ref: ") {
            let reference = reference.trim();
            if !reference.is_empty() {
                // Loose ref; packed refs are covered by `packed-refs`.
                paths.push(git_dir.join(reference));
                paths.push(git_dir.join("packed-refs"));
            }
        }
        paths.push(head);
    }
    paths.retain(|p| p.exists());
    paths
}

/// Resolves the git directory of the checkout, if any.
fn git_dir() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(manifest_dir())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if dir.is_empty() {
        return None;
    }
    let path = Path::new(&dir);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        manifest_dir().join(path)
    };
    path.exists().then_some(path)
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default())
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// Keeps the value on one line and free of characters that would break
/// `cargo:rustc-env` or shell-quote badly in `--version` output.
fn sanitize(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '+'))
        .collect()
}

/// Days-based civil-from-days conversion (Howard Hinnant's algorithm), so the
/// build script needs no date crate. Returns `(year, month, day)` in UTC.
#[allow(clippy::many_single_char_names)] // faithful to the published algorithm
fn civil_from_unix_seconds(secs: i64) -> (i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}
