//! Build provenance: what `kevin --version` prints and what
//! `plan/10-observability-ops.md` wants in the `kevin_build_info` metric and
//! the `kevin.startup.ready` log line.
//!
//! The values are stamped by `build.rs`. [`GIT_SHA`] is `unknown` when the
//! source tree carries no git metadata (release source tarball, vendored
//! build), so nothing here can fail at runtime.

/// Crate semver (`Cargo.toml` `workspace.package.version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Abbreviated commit id the binary was built from, or `unknown`.
pub const GIT_SHA: &str = env!("KEVIN_BUILD_GIT_SHA");

/// Build date, `YYYY-MM-DD` in UTC.
pub const BUILD_DATE: &str = env!("KEVIN_BUILD_DATE");

/// The string clap prints for `--version`, after the binary name:
/// `<semver> (<sha> <date>)`, e.g. `0.1.0 (a1b2c3d4e 2026-08-22)`.
///
/// Keep the shape stable: `kevin --version` output is parsed by operators,
/// release tooling and `crates/kevin-cli/tests/ac_ws21_release.rs`.
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("KEVIN_BUILD_GIT_SHA"),
    " ",
    env!("KEVIN_BUILD_DATE"),
    ")"
);

/// True when the build carries no git provenance.
#[must_use]
pub fn git_sha_is_known() -> bool {
    GIT_SHA != "unknown"
}

#[cfg(test)]
mod tests {
    use super::{BUILD_DATE, GIT_SHA, LONG_VERSION, VERSION};

    #[test]
    fn long_version_is_semver_sha_and_date() {
        assert_eq!(LONG_VERSION, format!("{VERSION} ({GIT_SHA} {BUILD_DATE})"));
    }

    #[test]
    fn build_date_is_iso_yyyy_mm_dd() {
        let parts: Vec<&str> = BUILD_DATE.split('-').collect();
        assert_eq!(parts.len(), 3, "build date {BUILD_DATE} is not YYYY-MM-DD");
        assert_eq!(parts[0].len(), 4, "year in {BUILD_DATE}");
        assert_eq!(parts[1].len(), 2, "month in {BUILD_DATE}");
        assert_eq!(parts[2].len(), 2, "day in {BUILD_DATE}");
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "{BUILD_DATE} is not numeric"
        );
    }
}
