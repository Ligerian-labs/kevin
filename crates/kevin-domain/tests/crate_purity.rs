//! `kevin-domain` stays pure: no tokio/sqlx/IO dependencies, no IO modules
//! in `src/` (`plan/12-workstreams.md` WS-01 acceptance 5).

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

const FORBIDDEN_DEPS: [&str; 12] = [
    "tokio",
    "tokio-util",
    "tokio-stream",
    "futures",
    "async-trait",
    "sqlx",
    "pgvector",
    "reqwest",
    "axum",
    "tower",
    "figment",
    "testcontainers",
];

const FORBIDDEN_PATHS: [&str; 5] = ["std::fs", "std::net", "std::process", "tokio::", "sqlx::"];

fn manifest_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn dependencies_section(manifest: &str) -> String {
    let mut in_deps = false;
    let mut out = String::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed == "[dependencies]";
            continue;
        }
        if in_deps {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[test]
fn ac_ws01_5_no_tokio_sqlx_or_io_dependencies() {
    let manifest = fs::read_to_string(manifest_dir().join("Cargo.toml")).unwrap();
    let deps = dependencies_section(&manifest);
    assert!(!deps.trim().is_empty(), "could not find [dependencies]");
    for dep in FORBIDDEN_DEPS {
        assert!(
            !deps.lines().any(|l| l.trim_start().starts_with(dep)),
            "kevin-domain must not depend on `{dep}` (found in [dependencies])"
        );
    }
    // Only the plan's allowed pure dependencies.
    let allowed = [
        "chrono",
        "rust_decimal",
        "serde",
        "serde_json",
        "thiserror",
        "uuid",
    ];
    for line in deps
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
    {
        let name = line.split(['=', '.']).next().unwrap().trim();
        assert!(
            allowed.contains(&name),
            "unexpected dependency `{name}` in kevin-domain"
        );
    }
    // No IO in the sources.
    let src = manifest_dir().join("src");
    let mut checked = 0;
    for entry in fs::read_dir(&src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            for forbidden in FORBIDDEN_PATHS {
                assert!(
                    !text.contains(forbidden),
                    "{} uses `{forbidden}`",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 16,
        "expected the domain modules to be scanned, got {checked}"
    );
}
