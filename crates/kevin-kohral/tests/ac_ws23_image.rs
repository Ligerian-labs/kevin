//! WS-23 acceptance criteria (`plan/12-workstreams.md` §WS-23): the Kohral
//! image and stack.
//!
//! > image boots with in-stack Postgres, runs migrations, seeds `MEMORY.md`,
//! > passes conformance in CI; container tier enables worker bypass flags only
//! > inside the image profile.
//!
//! "Boots and passes conformance" is proved by actually building the image and
//! running Kohral's `contract.py` against it — `deploy/kohral/README.md`
//! §Conformance and the `kohral-conformance` workflow do that, and `just ci`
//! deliberately starts no container. What is testable from here, and what
//! these tests pin, is everything that decides whether that run can succeed:
//!
//! * the image declares the layout, the CLIs, the port and the probe the plan
//!   requires, and every external pin is recorded with a digest;
//! * the entrypoint is idempotent for `MEMORY.md` and fails fast, with a
//!   message naming the secret, when the token or the database is missing;
//! * the conformance fragment is a *valid Kevin configuration* in the kohral
//!   profile with the fake worker as the only worker, and the scenario shipped
//!   next to it is byte-for-byte the one `kevin_kohral::conformance` runs;
//! * the container tier — and with it the bypass flags — is switched on by the
//!   image, not by anything an operator overlay or a checked-out repository
//!   can reach.

use std::path::{Path, PathBuf};
use std::process::Command;

use kevin_config::{LoadOptions, Profile, load};

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

/// Every needle must appear in `haystack`, and the assertion says which one did not.
fn assert_all(haystack: &str, what: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            haystack.contains(needle),
            "{what} must contain {needle:?}\n--- {what} ---\n{haystack}"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. The image bundles the coding-agent CLIs, pinned.
// ---------------------------------------------------------------------------

/// Unlike `deploy/Dockerfile` (the plain daemon), the Kohral image installs
/// Node and the four worker CLIs, and every version it installs is recorded
/// with a digest in `deploy/kohral/upstreams.lock.json`
/// (`plan/08-kohral-runtime.md` §6, `plan/09-security.md` §Supply chain).
#[test]
fn ac_ws23_1_image_installs_the_agent_clis_at_pinned_and_locked_versions() {
    let dockerfile = read("deploy/kohral/Dockerfile");
    let lock: serde_json::Value =
        serde_json::from_str(&read("deploy/kohral/upstreams.lock.json")).expect("lock is JSON");

    assert_all(
        &dockerfile,
        "the Kohral Dockerfile",
        &[
            "npm install --global",
            "@anthropic-ai/claude-code@${CLAUDE_VERSION}",
            "@openai/codex@${CODEX_VERSION}",
            "opencode-ai@${OPENCODE_VERSION}",
            "@earendil-works/pi-coding-agent@${PI_VERSION}",
            // The four binaries must actually be on PATH after the install.
            "claude --version; codex --version; opencode --version; pi --version",
        ],
    );

    // Node: pinned version *and* a checksum for both architectures.
    let node = &lock["node"];
    let version = node["version"].as_str().expect("node.version");
    assert!(
        dockerfile.contains(&format!("ARG NODE_VERSION={version}")),
        "the Dockerfile must pin the Node version from the lock ({version})"
    );
    for arch in ["linux-x64", "linux-arm64"] {
        let sha = node["artifacts"][arch]["sha256"]
            .as_str()
            .unwrap_or_else(|| panic!("node.artifacts.{arch}.sha256"));
        assert_eq!(sha.len(), 64, "{arch}: sha256 must be 64 hex characters");
        assert!(
            dockerfile.contains(sha),
            "the Dockerfile must verify the {arch} tarball against the locked sha256"
        );
    }
    assert!(
        dockerfile.contains("sha256sum -c -"),
        "downloads must be checksum-verified, not trusted"
    );

    // Every CLI: a package, an exact version and a registry integrity digest,
    // and the Dockerfile pins the same version.
    for (kind, arg) in [
        ("claude", "CLAUDE_VERSION"),
        ("codex", "CODEX_VERSION"),
        ("opencode", "OPENCODE_VERSION"),
        ("pi", "PI_VERSION"),
    ] {
        let entry = &lock["agent_clis"][kind];
        let package = entry["package"].as_str().expect("package");
        let version = entry["version"].as_str().expect("version");
        let integrity = entry["integrity"].as_str().expect("integrity");
        assert!(
            integrity.starts_with("sha512-"),
            "{kind}: the lock must record the npm integrity digest"
        );
        assert!(
            dockerfile.contains(&format!("ARG {arg}={version}")),
            "the Dockerfile must pin {kind} to the locked {version}"
        );
        assert!(
            dockerfile.contains(package),
            "the Dockerfile must install {package}"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Layout, port, probe, user.
// ---------------------------------------------------------------------------

/// `plan/08` §5 (volumes) and §6 (image): read-only config at
/// `/opt/kevin/config`, the writable volume at `/opt/kevin/data`, a non-root
/// uid 10000 matching Kohral's `VolumeSpec('data', …, 10000, 10000)`, the
/// Kohral bind port exposed and a healthcheck on the unauthenticated
/// `/health`.
#[test]
fn ac_ws23_2_image_lays_out_config_data_port_probe_and_a_non_root_uid() {
    let dockerfile = read("deploy/kohral/Dockerfile");
    assert_all(
        &dockerfile,
        "the Kohral Dockerfile",
        &[
            "--uid 10000",
            "--gid 10000",
            "chown -R 10000:10000 /opt/kevin/data",
            "USER 10000:10000",
            "mkdir -p /opt/kevin/config",
            "HOME=/opt/kevin/data/home",
            "KEVIN__KEVIN__DATA_DIR=/opt/kevin/data",
            "KEVIN__KOHRAL__BIND=0.0.0.0:8080",
            "VOLUME [\"/opt/kevin/data\"]",
            "EXPOSE 8080",
            "HEALTHCHECK",
            "http://127.0.0.1:8080/health",
            "ENTRYPOINT [\"/usr/local/bin/kevin-entrypoint\"]",
            "CMD [\"serve\", \"--kohral\"]",
        ],
    );
    assert!(
        !dockerfile.contains("USER root"),
        "the image must not end up running as root"
    );

    // The plain daemon image must stay the plain daemon image: it is the one
    // without the CLIs, and WS-23 does not smuggle them in.
    let daemon = read("deploy/Dockerfile");
    assert!(
        !daemon.contains("npm install"),
        "deploy/Dockerfile is the daemon image and must not bundle the agent CLIs"
    );
}

// ---------------------------------------------------------------------------
// 3. The stack.
// ---------------------------------------------------------------------------

/// `plan/08` §6: one isolated stack per agent = `gateway` + `memory`
/// (pgvector), the `data` and `memory-data` volumes, the secret bindings and a
/// private network — the Compose rendering of the `WorkloadSpec` Kohral's
/// provisioner applies.
#[test]
fn ac_ws23_3_stack_is_gateway_plus_pgvector_with_volumes_secrets_and_a_private_network() {
    let compose = read("deploy/kohral/compose.yml");
    assert_all(
        &compose,
        "deploy/kohral/compose.yml",
        &[
            "  gateway:",
            "  memory:",
            "pgvector/pgvector:pg16",
            "data:/opt/kevin/data",
            "./config:/opt/kevin/config:ro",
            "memory-data:/var/lib/postgresql/data",
            "kohral-runtime-token",
            "kevin-database-url",
            "postgres-password",
            "kevin-env",
            // Kohral mounts the DSN as a secret file, never as an env value.
            "KEVIN__DATABASE__URL_FILE: /run/secrets/kevin-database-url",
            "KOHRAL_COLLABORATION_URL",
            "condition: service_healthy",
            "pg_isready -U kevin -d kevin",
            "http://127.0.0.1:8080/health",
            "networks:",
        ],
    );
    // The gateway must reach Postgres by service name, never by a host port.
    assert!(
        compose.contains("POSTGRES_HOST: memory"),
        "the gateway talks to the in-stack Postgres by service name"
    );
    assert!(
        !compose.contains("5432:5432"),
        "Postgres must not be published outside the stack"
    );

    let conformance = read("deploy/kohral/compose.conformance.yaml");
    assert_all(
        &conformance,
        "deploy/kohral/compose.conformance.yaml",
        &[
            "KEVIN_CONFIG: /opt/kevin/config/conformance.toml",
            "pgvector/pgvector",
            // The crash phases decide when the gateway comes back, not Podman.
            "restart: \"no\"",
        ],
    );
}

// ---------------------------------------------------------------------------
// 4. The entrypoint.
// ---------------------------------------------------------------------------

/// Sources `entrypoint.sh` as a library and runs `script` with the functions
/// in scope. Returns (exit status, stdout, stderr).
fn entrypoint_sh(script: &str, env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    let path = repo_root().join("deploy/kohral/entrypoint.sh");
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!(
            ". {}\n{script}",
            path.display().to_string().replace(' ', "\\ ")
        ))
        .env("KEVIN_ENTRYPOINT_LIB", "1");
    for (key, value) in env {
        command.env(key, value);
    }
    let out = command.output().expect("running the entrypoint under sh");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `plan/08` §5.1 and Kohral `docs/07`: `MEMORY.md` belongs to the agent. The
/// entrypoint writes the documentation pointer **only when the file is
/// absent**, so a redeploy never discards what the agent recorded — which
/// means running it twice is a no-op.
#[test]
fn ac_ws23_4_entrypoint_seeds_memory_md_once_and_never_overwrites_it() {
    let dir = tempfile::tempdir().expect("temp data dir");
    let memory = dir.path().join("MEMORY.md");
    let memory_arg = memory.display().to_string();

    let (code, _, err) = entrypoint_sh(&format!("seed_memory_file '{memory_arg}'"), &[]);
    assert_eq!(code, Some(0), "first seed failed: {err}");
    let seeded = std::fs::read_to_string(&memory).expect("MEMORY.md was created");
    assert!(
        seeded.contains("KOHRAL_DOCUMENTATION.md"),
        "the seed must point at the platform documentation, got:\n{seeded}"
    );

    // The agent then writes its own memories over it.
    std::fs::write(
        &memory,
        "# Memory\n\n- the operator prefers short answers\n",
    )
    .expect("agent writes MEMORY.md");
    let (code, _, err) = entrypoint_sh(&format!("seed_memory_file '{memory_arg}'"), &[]);
    assert_eq!(code, Some(0), "second seed failed: {err}");
    assert_eq!(
        std::fs::read_to_string(&memory).expect("MEMORY.md still there"),
        "# Memory\n\n- the operator prefers short answers\n",
        "a redeploy must keep the agent's own memory file"
    );
    assert!(
        err.contains("keeping the agent's own"),
        "the second run should say it kept the file, stderr was:\n{err}"
    );
}

/// A missing runtime token or database is a deployment defect: the entrypoint
/// must say which secret is missing and exit before `kevin` ever starts,
/// rather than booting a gateway that answers 401 to every Kohral request
/// (`plan/10-observability-ops.md` §Startup: failure before `ready` is fatal).
#[test]
fn ac_ws23_5_entrypoint_fails_fast_with_a_message_naming_the_missing_secret() {
    let dir = tempfile::tempdir().expect("temp dir");
    let token = dir.path().join("kohral-runtime-token");
    let empty = dir.path().join("empty-token");
    std::fs::write(&token, "s3cret").expect("token file");
    std::fs::write(&empty, "").expect("empty token file");

    // No token at all.
    let (code, _, err) = entrypoint_sh(
        "require_inputs",
        &[
            ("KOHRAL_RUNTIME_TOKEN_FILE", "/nonexistent/token"),
            ("KEVIN__DATABASE__URL", "postgres://kevin@memory/kevin"),
        ],
    );
    assert_eq!(code, Some(3), "missing token must exit 3, stderr:\n{err}");
    assert!(
        err.contains("/nonexistent/token") && err.contains("KEVIN_RUNTIME_TOKEN"),
        "the error must name the path and the Kohral secret binding:\n{err}"
    );

    // Token file present but empty — every /v1 call would 401.
    let (code, _, err) = entrypoint_sh(
        "require_inputs",
        &[
            (
                "KOHRAL_RUNTIME_TOKEN_FILE",
                &empty.display().to_string() as &str,
            ),
            ("KEVIN__DATABASE__URL", "postgres://kevin@memory/kevin"),
        ],
    );
    assert_eq!(code, Some(3), "empty token must exit 3, stderr:\n{err}");
    assert!(
        err.contains("empty"),
        "the error must say it is empty:\n{err}"
    );

    // Token fine, no database.
    let (code, _, err) = entrypoint_sh(
        "require_inputs",
        &[(
            "KOHRAL_RUNTIME_TOKEN_FILE",
            &token.display().to_string() as &str,
        )],
    );
    assert_eq!(
        code,
        Some(3),
        "missing database must exit 3, stderr:\n{err}"
    );
    assert!(
        err.contains("KEVIN__DATABASE__URL"),
        "the error must name the variable to set:\n{err}"
    );

    // Both present: nothing to complain about.
    let (code, _, err) = entrypoint_sh(
        "require_inputs",
        &[
            (
                "KOHRAL_RUNTIME_TOKEN_FILE",
                &token.display().to_string() as &str,
            ),
            ("KEVIN__DATABASE__URL", "postgres://kevin@memory/kevin"),
            (
                "KEVIN_CONFIG_DIR",
                &dir.path().display().to_string() as &str,
            ),
        ],
    );
    assert_eq!(code, Some(0), "a complete deployment must pass: {err}");
}

/// Three ways the database arrives, in precedence order: the
/// `kevin-database-url` secret file Kohral's `KevinRuntimeStrategy` mounts
/// (`KEVIN__DATABASE__URL_FILE` — the password never reaches the environment),
/// an explicit `KEVIN__DATABASE__URL`, and finally `POSTGRES_*` plus the
/// password file for a hand-run stack. `database.url` and `database.url_file`
/// are mutually exclusive, so the file path must *not* also compose a URL.
#[test]
fn ac_ws23_6_entrypoint_resolves_the_database_from_the_secret_file_then_env_then_postgres_vars() {
    let dir = tempfile::tempdir().expect("temp dir");
    let password = dir.path().join("postgres-password");
    std::fs::write(&password, "hunter2").expect("password file");
    let password = password.display().to_string();
    let dsn = dir.path().join("kevin-database-url");
    std::fs::write(&dsn, "postgres://kevin:hunter2@memory:5432/kevin").expect("dsn file");
    let dsn = dsn.display().to_string();

    let show = "resolve_database; printf '%s|%s' \"${KEVIN__DATABASE__URL_FILE:-}\" \"${KEVIN__DATABASE__URL:-}\"";

    // Kohral's shape: the secret file wins and no URL is composed alongside it.
    let (code, out, err) = entrypoint_sh(
        show,
        &[
            ("KEVIN_DATABASE_URL_FILE", &dsn as &str),
            ("KEVIN_POSTGRES_PASSWORD_FILE", &password as &str),
            ("POSTGRES_HOST", "memory"),
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, format!("{dsn}|"), "the secret file must win alone");

    // Hand-run stack: compose from POSTGRES_* plus the password file.
    let (code, out, err) = entrypoint_sh(
        show,
        &[
            ("KEVIN_DATABASE_URL_FILE", "/nonexistent/dsn"),
            ("KEVIN_POSTGRES_PASSWORD_FILE", &password as &str),
            ("POSTGRES_HOST", "memory"),
            ("POSTGRES_USER", "kevin"),
            ("POSTGRES_DB", "kevin"),
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "|postgres://kevin:hunter2@memory:5432/kevin");

    // An explicit URL always wins over the POSTGRES_* composition.
    let (code, out, err) = entrypoint_sh(
        show,
        &[
            ("KEVIN_DATABASE_URL_FILE", "/nonexistent/dsn"),
            ("KEVIN_POSTGRES_PASSWORD_FILE", &password as &str),
            ("KEVIN__DATABASE__URL", "postgres://other/kevin"),
        ],
    );
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(out, "|postgres://other/kevin", "an explicit URL must win");
}

/// The entrypoint runs the migrations before `kevin serve --kohral`, and keeps
/// retrying while the `memory` service is still starting (`plan/08` §6).
///
/// It is not the only start path: Kohral's `KevinRuntimeStrategy` gives the
/// gateway service its own `/bin/sh -c "<seed MEMORY.md>; exec kevin serve
/// --kohral"`, which replaces the entrypoint entirely. Migrations must happen
/// there too, so the *image* — not this script — turns `auto_migrate` on.
#[test]
fn ac_ws23_7_entrypoint_migrates_then_execs_and_the_image_migrates_without_it() {
    let script = read("deploy/kohral/entrypoint.sh");
    assert_all(
        &script,
        "deploy/kohral/entrypoint.sh",
        &[
            "kevin db migrate",
            "exec kevin \"$@\"",
            "/run/secrets/kevin-env",
            "/run/secrets/postgres-password",
            "/run/secrets/kevin-database-url",
        ],
    );
    let migrate = script.find("kevin db migrate").expect("migration step");
    let exec = script.find("exec kevin").expect("exec step");
    assert!(
        migrate < exec,
        "migrations must run before the server starts"
    );

    assert!(
        read("deploy/kohral/Dockerfile").contains("KEVIN__DATABASE__AUTO_MIGRATE=true"),
        "the image must migrate on the start path that skips this entrypoint"
    );
}

// ---------------------------------------------------------------------------
// 5. The conformance profile.
// ---------------------------------------------------------------------------

/// The fragment mounted by `compose.conformance.yaml` must be a configuration
/// Kevin actually accepts, in the kohral profile, with the fake worker as the
/// only worker (`plan/08` §1.9). A typo here would only surface as a red CI
/// job with a container log to read.
#[test]
fn ac_ws23_8_conformance_fragment_is_a_valid_kohral_config_with_only_the_fake_worker() {
    let resolved = load(LoadOptions {
        config_file: Some(repo_root().join("deploy/kohral/conformance.toml")),
        env: vec![
            ("KEVIN__KEVIN__PROFILE".to_owned(), "kohral".to_owned()),
            (
                "KEVIN__KOHRAL__TOKEN_FILE".to_owned(),
                "/run/secrets/kohral-runtime-token".to_owned(),
            ),
        ],
        ..LoadOptions::hermetic()
    })
    .unwrap_or_else(|errors| panic!("the conformance fragment must load:\n{errors}"));
    let config = &resolved.config;

    assert_eq!(config.kevin.profile, Profile::Kohral);
    assert!(
        config.kohral.enabled,
        "the profile must enable the contract"
    );
    assert!(config.workers.fake.enabled, "the fake worker is the worker");
    for (name, enabled) in [
        ("claude", config.workers.claude.enabled),
        ("codex", config.workers.codex.enabled),
        ("pi", config.workers.pi.enabled),
        ("opencode", config.workers.opencode.enabled),
    ] {
        assert!(!enabled, "{name} must be off in the conformance profile");
    }
    for (role, alias) in config.roles.bindings() {
        assert_eq!(
            alias.as_str(),
            "fake",
            "{role} must route to the fake worker"
        );
    }
    for (kind, routing) in &config.routing.kinds {
        for alias in &routing.candidates {
            assert_eq!(
                alias.as_str(),
                "fake",
                "routing.kinds.{kind} must only offer the fake worker"
            );
        }
    }
    assert!(
        !config.memory.enabled,
        "conformance must not depend on an embedding model"
    );

    // `workers.fake.enabled` is also what flips the ledger to "answer only",
    // which is what makes `contract.py`'s `output == "kohral-ok"` hold.
    assert_eq!(
        kevin_kohral::projection::Narrative::for_config(config),
        kevin_kohral::projection::Narrative::AnswerOnly
    );

    // The scenario the fragment points at is the one that ships next to it.
    assert_eq!(
        config.workers.fake.script,
        PathBuf::from("/opt/kevin/config/conformance-scenario.json")
    );
}

/// The scenario file is the executable form of `plan/08` §1.9 — and it must
/// stay identical to the one the in-process suite
/// (`kevin_kohral::conformance::scenario`) runs, or the container and the
/// embedded gateway would be judged against different fakes.
#[test]
fn ac_ws23_9_shipped_scenario_matches_the_conformance_scenario() {
    let shipped: serde_json::Value =
        serde_json::from_str(&read("deploy/kohral/conformance-scenario.json"))
            .expect("the shipped scenario is JSON");
    let expected = serde_json::to_value(kevin_kohral::conformance::scenario())
        .expect("the scenario serialises");
    assert_eq!(
        shipped, expected,
        "regenerate deploy/kohral/conformance-scenario.json from \
         kevin_kohral::conformance::scenario()"
    );

    // The two hooks the contract phases depend on, spelled out.
    let text = read("deploy/kohral/conformance-scenario.json");
    assert_all(
        &text,
        "the conformance scenario",
        &["[[KOHRAL_HOLD]]", "kohral-ok", "planner.understanding"],
    );
}

// ---------------------------------------------------------------------------
// 6. Container tier.
// ---------------------------------------------------------------------------

/// "container tier enables worker bypass flags only inside the image profile"
/// (`plan/12` WS-23, `plan/09-security.md` §Sandbox tiers): the tier is set by
/// the image's own environment — not by the operator overlay (Kohral protects
/// `sandbox`) and not by a repository Kevin happens to check out (the project
/// layer may not touch `sandbox.*`/`workers.*`). Outside the image the default
/// stays `cli-native`, where the same flags are rejected.
#[test]
fn ac_ws23_10_bypass_flags_are_enabled_by_the_image_tier_and_nowhere_else() {
    let dockerfile = read("deploy/kohral/Dockerfile");
    assert_all(
        &dockerfile,
        "the Kohral Dockerfile",
        &[
            "KEVIN__SANDBOX__TIER=container",
            "KEVIN__WORKERS__CLAUDE__PERMISSION_MODE=bypassPermissions",
        ],
    );
    assert!(
        !dockerfile.contains("KEVIN__WORKERS__CODEX__SANDBOX=danger-full-access"),
        "codex stays at workspace-write inside the stack (plan/08 §6)"
    );

    // What the image environment produces: the container tier, the bypass flag
    // accepted, and the dangerous-flag gate derived from the tier.
    let image_env = |extra: Vec<(String, String)>| {
        let mut env = vec![
            ("KEVIN__KEVIN__PROFILE".to_owned(), "kohral".to_owned()),
            (
                "KEVIN__KOHRAL__TOKEN_FILE".to_owned(),
                "/run/secrets/kohral-runtime-token".to_owned(),
            ),
            (
                "KEVIN__DATABASE__URL".to_owned(),
                "postgres://kevin@memory/kevin".to_owned(),
            ),
            ("KEVIN__SANDBOX__TIER".to_owned(), "container".to_owned()),
            (
                "KEVIN__WORKERS__CLAUDE__PERMISSION_MODE".to_owned(),
                "bypassPermissions".to_owned(),
            ),
        ];
        env.extend(extra);
        load(LoadOptions {
            env,
            ..LoadOptions::hermetic()
        })
    };
    let resolved = image_env(Vec::new())
        .unwrap_or_else(|errors| panic!("the image environment must be valid:\n{errors}"));
    assert_eq!(
        resolved.config.sandbox.tier,
        kevin_config::SandboxTier::Container
    );
    assert!(
        resolved.config.sandbox.allow_dangerous_flags,
        "the container tier is what allows the bypass flags"
    );

    // The same permission mode without the image's tier is a config error.
    let outside = load(LoadOptions {
        env: vec![
            (
                "KEVIN__WORKERS__CLAUDE__PERMISSION_MODE".to_owned(),
                "bypassPermissions".to_owned(),
            ),
            (
                "KEVIN__DATABASE__URL".to_owned(),
                "postgres://kevin@localhost/kevin".to_owned(),
            ),
        ],
        ..LoadOptions::hermetic()
    });
    let errors = outside.expect_err("bypassPermissions outside the container tier must be refused");
    assert!(
        errors.to_string().contains("bypassPermissions"),
        "the error must be about the bypass flag:\n{errors}"
    );
}

// ---------------------------------------------------------------------------
// 7. CI.
// ---------------------------------------------------------------------------

/// `plan/08` §8: a CI job that builds the image, starts the stack with the
/// fake worker and runs the three `contract.py` phases — and skips cleanly
/// when the Kohral checkout is unavailable, because Kohral is a separate
/// private repository. The existing `just ci` jobs stay container-free.
#[test]
fn ac_ws23_11_ci_runs_the_three_contract_phases_and_skips_without_kohral() {
    let workflow = read(".github/workflows/kohral-conformance.yml");
    assert_all(
        &workflow,
        "the kohral-conformance workflow",
        &[
            "kohral-conformance",
            "deploy/kohral/Dockerfile",
            "compose.conformance.yaml",
            "--runtime hermes",
            "contract.py",
            "basic",
            "accept-crash",
            "verify-crash",
            "--run-id-file",
            // The suite is skipped, not failed, when contract.py is missing.
            "skipped",
        ],
    );
    assert!(
        workflow.contains("crates/kevin-kohral/**") && workflow.contains("deploy/kohral/**"),
        "the job must run on changes to the runtime crate and the image"
    );

    // `just ci` must stay fast and must not start containers.
    let ci = read(".github/workflows/ci.yml");
    for forbidden in ["podman", "docker build", "compose"] {
        assert!(
            !ci.contains(forbidden),
            "ci.yml must stay container-free (found {forbidden:?})"
        );
    }
}
