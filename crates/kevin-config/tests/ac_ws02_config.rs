//! WS-02 acceptance criteria (`plan/12-workstreams.md` §WS-02) plus the
//! property tests `plan/11-testing.md` asks of `kevin-config`.

// Test helpers panic on broken fixtures; that is the intended behaviour.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kevin_config::{
    ClaudePermissionMode, ConfigError, DEFAULT_TOML, KevinConfig, LoadOptions, Profile, Resolved,
    SandboxTier, Source, load,
};
use kevin_domain::{ModelAlias, WorkerKind};
use proptest::prelude::*;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).expect("fixture exists")
}

fn alias(s: &str) -> ModelAlias {
    ModelAlias::new(s).unwrap()
}

/// A temp repo: `<root>/.git/`, `<root>/.kevin/kevin.toml` = `project`, and a
/// nested working directory.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    nested: PathBuf,
}

impl Repo {
    fn new(project_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let nested = root.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".kevin")).unwrap();
        std::fs::write(root.join(".kevin/kevin.toml"), project_toml).unwrap();
        Self {
            _dir: dir,
            root,
            nested,
        }
    }

    fn project_file(&self) -> PathBuf {
        self.root.join(".kevin/kevin.toml")
    }
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// Leaf paths whose values differ between two resolved configs.
fn diff_keys(a: &Resolved, b: &Resolved) -> BTreeSet<String> {
    fn leaves(t: &toml::Table, prefix: &str, out: &mut Vec<(String, toml::Value)>) {
        for (k, v) in t {
            let p = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            match v {
                toml::Value::Table(t) if !t.is_empty() => leaves(t, &p, out),
                _ => out.push((p, v.clone())),
            }
        }
    }
    let mut la = Vec::new();
    let mut lb = Vec::new();
    leaves(&toml::Table::try_from(&a.config).unwrap(), "", &mut la);
    leaves(&toml::Table::try_from(&b.config).unwrap(), "", &mut lb);
    let ma: std::collections::BTreeMap<_, _> = la.into_iter().collect();
    let mb: std::collections::BTreeMap<_, _> = lb.into_iter().collect();
    ma.iter()
        .filter(|(k, v)| mb.get(*k) != Some(v))
        .map(|(k, _)| k.clone())
        .chain(mb.keys().filter(|k| !ma.contains_key(*k)).cloned())
        .collect()
}

// ---------------------------------------------------------------------------
// AC 1 — defaults deserialize from the TOML block in plan/03 byte-for-byte
// ---------------------------------------------------------------------------

#[test]
fn ac_ws02_1_defaults_equal_plan_toml_block() {
    // The embedded asset is the plan's block, byte for byte.
    let plan = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plan/03-config-schema.md"),
    )
    .expect("plan/03 is in the repo");
    let block = plan
        .split("## Full schema with defaults")
        .nth(1)
        .and_then(|rest| rest.split("```toml\n").nth(1))
        .and_then(|rest| rest.split("```").next())
        .expect("plan/03 has the schema block");
    assert_eq!(
        DEFAULT_TOML, block,
        "assets/default.toml must be the plan/03 block verbatim"
    );

    // Parsing it yields exactly KevinConfig::default() …
    let parsed: KevinConfig = toml::from_str(DEFAULT_TOML).expect("default TOML parses");
    assert_eq!(parsed, KevinConfig::default());

    // … the default round-trips through TOML …
    let rendered = toml::to_string_pretty(&KevinConfig::default()).unwrap();
    let back: KevinConfig = toml::from_str(&rendered).unwrap();
    assert_eq!(back, KevinConfig::default());

    // … and the hermetic load is the default plus the derived flag.
    let resolved = load(LoadOptions::hermetic()).expect("defaults are valid");
    assert_eq!(resolved.config, KevinConfig::default());
    assert!(
        resolved
            .sources
            .values()
            .all(|s| matches!(s, Source::Default | Source::Derived))
    );
    assert_eq!(
        resolved.source_of("sandbox.allow_dangerous_flags"),
        Source::Derived
    );

    insta::assert_snapshot!("default_config_toml", rendered);
    insta::assert_snapshot!("default_config_show_redacted", resolved.redacted_toml());
}

#[test]
fn ac_ws02_1_model_catalog_keeps_worker_specific_extras() {
    let cfg = KevinConfig::default();
    let pi = &cfg.models[&alias("sonnet5-pi")];
    assert_eq!(pi.worker, WorkerKind::Pi);
    assert_eq!(pi.provider(), Some("anthropic"));
    assert_eq!(
        pi.extra.get("provider"),
        Some(&toml::Value::String("anthropic".into()))
    );
    let codex = &cfg.models[&alias("gpt56-codex")];
    assert_eq!(codex.input_usd_per_m, None, "unknown prices stay unset");
    assert_eq!(cfg.models.len(), 8);
}

// ---------------------------------------------------------------------------
// AC 2 — precedence across all five layers
// ---------------------------------------------------------------------------

#[test]
fn ac_ws02_2_precedence_across_all_layers() {
    let repo = Repo::new(&fixture_text("project.toml"));
    let user = fixture("user.toml");
    let extra = fixture("extra.toml");

    // Each layer alone (on top of the lower ones) wins for kevin.instance_name.
    let base = LoadOptions::hermetic()
        .user_file(&user)
        .project_dir(&repo.nested);
    let r = load(base.clone()).unwrap();
    assert_eq!(r.config.kevin.instance_name, "project");
    assert_eq!(
        r.source_of("kevin.instance_name"),
        Source::ProjectFile(repo.project_file())
    );
    // Lower layers still contribute the keys the project file didn't set.
    assert_eq!(r.config.budget.max_attempts, 3);
    assert_eq!(
        r.source_of("budget.max_attempts"),
        Source::UserFile(user.clone())
    );
    assert_eq!(r.config.concurrency.per_worker_kind[&WorkerKind::Claude], 2);
    assert_eq!(r.config.concurrency.per_worker_kind[&WorkerKind::Codex], 4);
    assert_eq!(
        r.source_of("concurrency.per_worker_kind.codex"),
        Source::Default
    );
    assert_eq!(r.config.checks.commands, vec!["just ci"]);
    assert_eq!(
        r.config.routing.kinds[&kevin_domain::TaskKind::Implement].candidates,
        vec![alias("sonnet5-claude")]
    );
    // Deep-merged model entry: one key patched, the rest from the defaults.
    let sonnet = &r.config.models[&alias("sonnet5-claude")];
    assert_eq!(sonnet.input_usd_per_m.unwrap().to_string(), "2.5");
    assert_eq!(sonnet.model, "claude-sonnet-5");
    assert_eq!(
        r.source_of("models.sonnet5-claude.input_usd_per_m"),
        Source::UserFile(user.clone())
    );
    assert_eq!(r.source_of("models.sonnet5-claude.model"), Source::Default);
    assert!(r.config.models.contains_key(&alias("custom-pi")));

    let r = load(base.clone().config_file(&extra)).unwrap();
    assert_eq!(r.config.kevin.instance_name, "file");
    assert_eq!(r.config.telemetry.log_level, "debug");
    assert_eq!(
        r.source_of("kevin.instance_name"),
        Source::ConfigFile(extra.clone())
    );

    // $KEVIN_CONFIG is the same layer as --config when the flag is absent.
    let r = load(base.clone().env("KEVIN_CONFIG", extra.to_string_lossy())).unwrap();
    assert_eq!(r.config.kevin.instance_name, "file");

    let r = load(
        base.clone()
            .config_file(&extra)
            .env("KEVIN__KEVIN__INSTANCE_NAME", "env")
            .env("KEVIN__DATABASE__URL", "postgres://u:p@db.example:5432/k")
            .env("KEVIN__BUDGET__MAX_ATTEMPTS", "5")
            .env("KEVIN__WORKERS__FAKE__ENABLED", "true")
            .env("KEVIN__SERVER__CORS_ORIGINS", "[\"https://a.example\"]"),
    )
    .unwrap();
    assert_eq!(r.config.kevin.instance_name, "env");
    assert_eq!(
        r.source_of("kevin.instance_name"),
        Source::Env("KEVIN__KEVIN__INSTANCE_NAME".into())
    );
    assert_eq!(r.config.database.url, "postgres://u:p@db.example:5432/k");
    assert_eq!(r.config.budget.max_attempts, 5);
    assert!(r.config.workers.fake.enabled);
    assert_eq!(r.config.server.cors_origins, vec!["https://a.example"]);

    let r = load(
        base.config_file(&extra)
            .env("KEVIN__KEVIN__INSTANCE_NAME", "env")
            .set("kevin.instance_name=set")
            .set("budget.max_parallel_tasks=9"),
    )
    .unwrap();
    assert_eq!(r.config.kevin.instance_name, "set");
    assert_eq!(r.source_of("kevin.instance_name"), Source::Set);
    assert_eq!(r.config.budget.max_parallel_tasks, 9);
    // Untouched keys keep their provenance all the way down.
    assert_eq!(
        r.source_of("telemetry.log_level"),
        Source::ConfigFile(extra)
    );
    assert_eq!(
        r.source_of("checks.commands"),
        Source::ProjectFile(repo.project_file())
    );
    assert_eq!(
        r.source_of("workers.claude.max_turns"),
        Source::UserFile(user)
    );
    assert_eq!(r.source_of("server.bind"), Source::Default);
}

#[test]
fn ac_ws02_2_missing_user_file_is_skipped_but_missing_config_flag_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let r = load(LoadOptions::hermetic().user_file(dir.path().join("nope.toml"))).unwrap();
    assert_eq!(r.config, KevinConfig::default());

    let err = load(LoadOptions::hermetic().config_file(dir.path().join("nope.toml"))).unwrap_err();
    assert!(
        matches!(err.0.as_slice(), [ConfigError::Io { .. }]),
        "{err}"
    );
}

#[test]
fn ac_ws02_2_kohral_env_aliases_map_to_kohral_keys() {
    let r = load(
        LoadOptions::hermetic()
            .env("KOHRAL_COLLABORATION_URL", "https://collab.example")
            .env("KOHRAL_RUNTIME_TOKEN_FILE", "/mnt/tok"),
    )
    .unwrap();
    assert_eq!(r.config.kohral.collaboration_url, "https://collab.example");
    assert_eq!(r.config.kohral.token_file, PathBuf::from("/mnt/tok"));
    assert_eq!(
        r.source_of("kohral.token_file"),
        Source::Env("KOHRAL_RUNTIME_TOKEN_FILE".into())
    );
    // KEVIN__KOHRAL__* beats the alias.
    let r = load(
        LoadOptions::hermetic()
            .env("KOHRAL_RUNTIME_TOKEN_FILE", "/mnt/tok")
            .env("KEVIN__KOHRAL__TOKEN_FILE", "/mnt/other"),
    )
    .unwrap();
    assert_eq!(r.config.kohral.token_file, PathBuf::from("/mnt/other"));
}

proptest! {
    /// Random subsets of layers, each setting `kevin.instance_name` to its own
    /// name: the highest present layer always wins.
    #[test]
    fn ac_ws02_2_prop_highest_present_layer_wins(mask in 0u8..32) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::new("[kevin]\ninstance_name = \"project\"\n");
        let mut opts = LoadOptions::hermetic();
        let mut expected = "kevin";
        if mask & 1 != 0 {
            opts = opts.user_file(write(dir.path(), "user.toml", "[kevin]\ninstance_name = \"user\"\n"));
            expected = "user";
        }
        if mask & 2 != 0 {
            opts = opts.project_dir(&repo.nested);
            expected = "project";
        }
        if mask & 4 != 0 {
            opts = opts.config_file(write(dir.path(), "extra.toml", "[kevin]\ninstance_name = \"file\"\n"));
            expected = "file";
        }
        if mask & 8 != 0 {
            opts = opts.env("KEVIN__KEVIN__INSTANCE_NAME", "env");
            expected = "env";
        }
        if mask & 16 != 0 {
            opts = opts.set("kevin.instance_name=set");
            expected = "set";
        }
        let r = load(opts).unwrap();
        prop_assert_eq!(r.config.kevin.instance_name.as_str(), expected);
    }

    /// Any valid config survives a TOML round-trip (here: defaults with a
    /// random sample of overrides).
    #[test]
    fn ac_ws02_2_prop_valid_config_round_trips(
        attempts in 1u8..10,
        parallel in 1u16..64,
        name in "[a-z][a-z0-9-]{0,15}",
        docs in any::<bool>(),
    ) {
        let mut cfg = KevinConfig::default();
        cfg.budget.max_attempts = attempts;
        cfg.budget.max_parallel_tasks = parallel;
        cfg.kevin.instance_name = name;
        cfg.server.docs = docs;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: KevinConfig = toml::from_str(&text).unwrap();
        prop_assert_eq!(back, cfg);
    }
}

// ---------------------------------------------------------------------------
// AC 3 — every validation rule has a failing config; errors are aggregated
// ---------------------------------------------------------------------------

fn load_err(opts: LoadOptions) -> Vec<ConfigError> {
    load(opts).expect_err("config must be rejected").0
}

fn has(errors: &[ConfigError], pred: impl Fn(&ConfigError) -> bool, what: &str) {
    assert!(
        errors.iter().any(pred),
        "expected an error for {what}; got:\n{}",
        errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn ac_ws02_3_all_validation_rules_are_reported_together() {
    let errors = load_err(LoadOptions::hermetic().config_file(fixture("invalid-everything.toml")));
    let source = Source::ConfigFile(fixture("invalid-everything.toml"));

    // database.url must be postgres://
    has(
        &errors,
        |e| matches!(e, ConfigError::InvalidDatabaseUrl { layer, .. } if *layer == source),
        "mysql url",
    );
    // roles / candidates must name existing aliases whose worker is enabled
    has(
        &errors,
        |e| matches!(e, ConfigError::UnknownModelAlias { key, alias, .. } if key == "roles.planner" && alias.as_str() == "does-not-exist"),
        "roles.planner",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::ModelWorkerDisabled { key, worker: WorkerKind::Codex, .. } if key == "roles.judge"),
        "roles.judge → disabled codex",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::UnknownModelAlias { key, .. } if key == "routing.kinds.implement.candidates[1]"),
        "candidates[1] ghost",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::ModelWorkerDisabled { key, .. } if key == "routing.kinds.implement.candidates[2]"),
        "candidates[2] disabled codex",
    );
    // pi aliases need provider; model must be non-empty
    has(
        &errors,
        |e| matches!(e, ConfigError::InvalidModelEntry { alias, .. } if alias.as_str() == "nopro-pi"),
        "pi without provider",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::InvalidModelEntry { alias, .. } if alias.as_str() == "empty-model"),
        "empty model id",
    );
    // dangerous flags outside container tier
    for key in [
        "workers.claude.permission_mode",
        "workers.codex.sandbox",
        "workers.codex.extra_args[0]",
        "workers.opencode.extra_args[0]",
        "sandbox.allow_dangerous_flags",
        "sandbox.network",
    ] {
        has(
            &errors,
            |e| matches!(e, ConfigError::ForbiddenOutsideContainer { key: k, .. } if k == key),
            key,
        );
    }
    // budgets > 0, max_parallel_tasks ≥ 1, other ranges
    for key in [
        "budget.default_run_usd",
        "budget.default_task_usd",
        "budget.default_run_wall",
        "budget.default_task_wall",
        "budget.max_attempts",
        "budget.max_parallel_tasks",
        "budget.max_tokens_per_task",
        "database.pool_size",
        "concurrency.per_worker_kind.codex",
        "routing.exploration",
        "routing.quality_weight",
        "memory.min_similarity",
    ] {
        has(
            &errors,
            |e| matches!(e, ConfigError::OutOfRange { key: k, .. } if k == key),
            key,
        );
    }
    // memory.dimensions must match the embedder model
    has(
        &errors,
        |e| {
            matches!(
                e,
                ConfigError::EmbeddingDimensionMismatch {
                    expected: 384,
                    actual: 768,
                    ..
                }
            )
        },
        "dimensions",
    );
    // non-loopback bind without a token file
    has(
        &errors,
        |e| matches!(e, ConfigError::InsecureBind { bind, .. } if bind == "0.0.0.0:7777"),
        "insecure bind",
    );

    // Every error carries the layer that produced it (the file, or the default it broke).
    assert!(
        errors.iter().all(|e| {
            let t = e.to_string();
            t.contains("(file:") || t.contains("(default)")
        }),
        "every error names its source"
    );
    assert!(errors.len() >= 27, "{} errors aggregated", errors.len());
}

#[test]
fn ac_ws02_3_unknown_keys_and_bad_types_are_attributed_to_their_layer() {
    let dir = tempfile::tempdir().unwrap();
    let user = write(dir.path(), "user.toml", "[kevin]\nprofil = \"server\"\n");
    let extra = write(
        dir.path(),
        "extra.toml",
        "[workers.claude]\nmax_turns = \"many\"\n[budget]\ndefault_run_wall = \"soon\"\n",
    );
    let errors = load_err(
        LoadOptions::hermetic()
            .user_file(&user)
            .config_file(&extra)
            .env("KEVIN__KEVIN__PROFILE", "cloud")
            .set("models.opus5-claude.worker=gemini")
            .set("routing.kinds.deploy.candidates=[\"fake\"]")
            .set("sandbox.tier=nope")
            .set("broken"),
    );
    let text = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer, .. } if key == "kevin.profil" && *layer == Source::UserFile(user.clone())),
        "unknown key kevin.profil",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer, .. } if key == "workers.claude.max_turns" && *layer == Source::ConfigFile(extra.clone())),
        "bad type max_turns",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer, .. } if key == "kevin.profile" && *layer == Source::Env("KEVIN__KEVIN__PROFILE".into())),
        "bad enum profile",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, .. } if key == "models.opus5-claude.worker"),
        "unknown worker kind",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer: Source::Set, .. } if key.starts_with("routing.kinds")),
        "unknown task kind",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer: Source::Set, .. } if key == "sandbox.tier"),
        "bad tier",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::InvalidSet { arg, .. } if arg == "broken"),
        "malformed --set",
    );
    // Both offending keys of the same file are reported, not just the first.
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, message, .. } if key == "budget.default_run_wall" && message.contains("invalid duration")),
        "bad duration",
    );
    assert!(text.contains("unknown field `profil`"), "{text}");
}

#[test]
fn ac_ws02_3_bad_duration_is_reported_with_key_path() {
    let errors = load_err(LoadOptions::hermetic().set("kevin.shutdown_grace_period=30"));
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, layer: Source::Set, message } if key == "kevin.shutdown_grace_period" && message.contains("invalid duration")),
        "bare number duration",
    );
    let errors = load_err(LoadOptions::hermetic().set("budget.default_run_wall=two hours"));
    has(
        &errors,
        |e| matches!(e, ConfigError::Invalid { key, message, .. } if key == "budget.default_run_wall" && message.contains("invalid duration")),
        "garbage duration",
    );
    let r = load(LoadOptions::hermetic().set("budget.default_run_wall=1h 30m")).unwrap();
    assert_eq!(r.config.budget.default_run_wall, Duration::from_mins(90));
}

#[test]
fn ac_ws02_3_project_layer_may_not_touch_protected_sections() {
    let repo = Repo::new(&fixture_text("project-protected.toml"));
    let errors = load_err(LoadOptions::hermetic().project_dir(&repo.nested));
    let project = Source::ProjectFile(repo.project_file());
    for key in [
        "sandbox.tier",
        "workers.claude.permission_mode",
        "workers.claude.extra_args",
        "workers.fake.enabled",
        "server.bind",
        "database.url",
        "kohral.enabled",
    ] {
        has(
            &errors,
            |e| matches!(e, ConfigError::ProjectLayerNotAllowed { key: k, layer } if k == key && *layer == project),
            key,
        );
    }
    assert!(
        errors
            .iter()
            .all(|e| matches!(e, ConfigError::ProjectLayerNotAllowed { .. })),
        "protected keys are rejected, not applied (so no follow-on errors): {errors:?}"
    );
    // The unprotected key in the same file still loads once the protected ones are gone.
    let repo = Repo::new("[kevin]\ninstance_name = \"fine\"\n[checks]\ncommands = [\"just ci\"]\n");
    let r = load(LoadOptions::hermetic().project_dir(&repo.nested)).unwrap();
    assert_eq!(r.config.kevin.instance_name, "fine");
}

#[test]
fn ac_ws02_3_insecure_bind_requires_token_file_or_kohral_profile() {
    let errors = load_err(
        LoadOptions::hermetic()
            .set("server.bind=0.0.0.0:7777")
            .set("server.auth_token_file="),
    );
    has(
        &errors,
        |e| {
            matches!(
                e,
                ConfigError::InsecureBind {
                    layer: Source::Set,
                    ..
                }
            )
        },
        "insecure bind",
    );
    // Loopback never needs a token file; a token file makes any bind fine.
    load(LoadOptions::hermetic().set("server.auth_token_file=")).unwrap();
    load(LoadOptions::hermetic().set("server.bind=[::]:7777")).unwrap();
    // The kohral profile satisfies it via kohral.token_file even without an API token file.
    let r = load(
        LoadOptions::hermetic()
            .set("kevin.profile=kohral")
            .set("server.auth_token_file="),
    )
    .unwrap();
    assert_eq!(r.config.server.bind.to_string(), "0.0.0.0:7777");
    let errors = load_err(
        LoadOptions::hermetic()
            .set("kevin.profile=kohral")
            .set("server.auth_token_file=")
            .set("kohral.token_file="),
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::InsecureBind { .. }),
        "kohral without any token",
    );
}

#[test]
fn ac_ws02_3_database_url_exactly_one_of_url_and_url_file() {
    // url_file alone (default url untouched) is fine and wins.
    let r = load(LoadOptions::hermetic().set("database.url_file=/run/secrets/db")).unwrap();
    assert_eq!(
        r.config.database.url_file,
        Some(PathBuf::from("/run/secrets/db"))
    );
    // url_file + explicit url → error.
    let errors = load_err(
        LoadOptions::hermetic()
            .set("database.url_file=/run/secrets/db")
            .env("KEVIN__DATABASE__URL", "postgres://a@b/c"),
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::DatabaseUrlExactlyOne { .. }),
        "both set",
    );
    // neither → error.
    let errors = load_err(LoadOptions::hermetic().set("database.url="));
    has(
        &errors,
        |e| matches!(e, ConfigError::DatabaseUrlExactlyOne { .. }),
        "neither set",
    );
    // postgresql:// scheme is accepted.
    load(LoadOptions::hermetic().set("database.url=postgresql://kevin@localhost/kevin")).unwrap();
}

#[test]
fn ac_ws02_3_container_tier_allows_dangerous_flags_and_derives_allow_flag() {
    let r = load(
        LoadOptions::hermetic()
            .set("sandbox.tier=container")
            .set("sandbox.network=deny")
            .set("workers.claude.permission_mode=bypassPermissions")
            .set("workers.codex.sandbox=danger-full-access")
            .set("workers.opencode.extra_args=[\"--auto\"]"),
    )
    .unwrap();
    assert_eq!(r.config.sandbox.tier, SandboxTier::Container);
    assert!(
        r.config.sandbox.allow_dangerous_flags,
        "derived from the tier"
    );
    assert_eq!(
        r.source_of("sandbox.allow_dangerous_flags"),
        Source::Derived
    );
    assert_eq!(
        r.config.workers.claude.permission_mode,
        ClaudePermissionMode::BypassPermissions
    );

    // Explicit false in container tier is respected (stricter is always allowed).
    let r = load(
        LoadOptions::hermetic()
            .set("sandbox.tier=container")
            .set("sandbox.allow_dangerous_flags=false"),
    )
    .unwrap();
    assert!(!r.config.sandbox.allow_dangerous_flags);
    assert_eq!(r.source_of("sandbox.allow_dangerous_flags"), Source::Set);

    // `none` tier still forbids the flags at config level (plan/03 says container only).
    let errors = load_err(
        LoadOptions::hermetic()
            .set("sandbox.tier=none")
            .set("workers.claude.permission_mode=bypassPermissions"),
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::ForbiddenOutsideContainer { tier, .. } if tier == "none"),
        "none tier",
    );
}

#[test]
fn ac_ws02_3_disabling_a_worker_breaks_aliases_that_use_it() {
    let errors = load_err(LoadOptions::hermetic().set("workers.claude.enabled=false"));
    has(
        &errors,
        |e| matches!(e, ConfigError::ModelWorkerDisabled { key, .. } if key == "roles.planner"),
        "planner",
    );
    has(
        &errors,
        |e| matches!(e, ConfigError::ModelWorkerDisabled { key, .. } if key == "routing.kinds.implement.candidates[0]"),
        "implement[0]",
    );
    // Rebinding the roles and candidates to an enabled worker fixes it.
    let mut opts = LoadOptions::hermetic()
        .set("workers.claude.enabled=false")
        .set("workers.fake.enabled=true")
        .set("roles.planner=fake")
        .set("roles.clarifier=fake")
        .set("roles.judge=fake")
        .set("roles.integrator=fake")
        .set("roles.default=fake");
    for kind in KevinConfig::default().routing.kinds.keys() {
        opts = opts.set(format!("routing.kinds.{kind}.candidates=[\"fake\"]"));
    }
    let r = load(opts).unwrap();
    assert_eq!(r.config.roles.planner, alias("fake"));
    assert!(
        r.config
            .routing
            .kinds
            .values()
            .all(|k| k.candidates == vec![alias("fake")])
    );
}

#[test]
fn ac_ws02_3_memory_dimensions_must_match_known_model() {
    let errors = load_err(LoadOptions::hermetic().set("memory.dimensions=512"));
    has(
        &errors,
        |e| {
            matches!(
                e,
                ConfigError::EmbeddingDimensionMismatch {
                    expected: 384,
                    actual: 512,
                    ..
                }
            )
        },
        "mismatch",
    );
    // Unknown model: no known dimension → accepted; embedder none → not checked.
    load(
        LoadOptions::hermetic()
            .set("memory.embedding_model=acme/embed-9000")
            .set("memory.dimensions=512"),
    )
    .unwrap();
    load(
        LoadOptions::hermetic()
            .set("memory.embedder=none")
            .set("memory.dimensions=512"),
    )
    .unwrap();
    load(
        LoadOptions::hermetic()
            .set("memory.embedding_model=BAAI/bge-base-en-v1.5")
            .set("memory.dimensions=768"),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// AC 4 — `config show` redacts and shows sources (library side; CLI side in kevin-cli)
// ---------------------------------------------------------------------------

#[test]
fn ac_ws02_4_redacted_output_hides_secrets_and_names_sources() {
    let r = load(
        LoadOptions::hermetic()
            .set("database.url=postgres://kevin:hunter2@db.example:5432/kevin")
            .set("server.auth_token_file=/etc/kevin/api.token")
            .set("kohral.identity_file=/run/secrets/id")
            .set("client.server_url=https://kevin.example")
            .env("KEVIN__KEVIN__PROFILE", "server"),
    )
    .unwrap();

    let toml_text = r.redacted_toml();
    assert!(!toml_text.contains("hunter2"), "{toml_text}");
    assert!(toml_text.contains("postgres://kevin:***@db.example:5432/kevin"));
    assert!(!toml_text.contains("/etc/kevin/api.token"));
    assert!(toml_text.contains("auth_token_file = \"***\""));
    assert!(toml_text.contains("token_file = \"***\""));
    assert!(toml_text.contains("identity_file = \"***\""));
    assert!(
        toml_text.contains("server_url = \"https://kevin.example\""),
        "non-secrets stay"
    );
    // env_passthrough *values* are variable names, not secrets.
    assert!(toml_text.contains("\"ANTHROPIC_API_KEY\""));
    // The redacted text is still valid TOML that parses into the schema.
    let _: KevinConfig = toml::from_str(&toml_text).unwrap();

    let with_sources = r.redacted_toml_with_sources();
    assert!(!with_sources.contains("hunter2"));
    assert!(with_sources.contains("# --set"), "{with_sources}");
    assert!(with_sources.contains("# env:KEVIN__KEVIN__PROFILE"));
    assert!(with_sources.contains("# profile:server"));
    assert!(with_sources.contains("# default"));
    assert!(with_sources.contains("# derived"));
    assert!(
        with_sources
            .lines()
            .any(|l| l.starts_with("database.url = \"postgres://kevin:***@"))
    );

    let json = r.redacted_json();
    assert_eq!(
        json["sources"]["kevin.profile"],
        "env:KEVIN__KEVIN__PROFILE"
    );
    assert_eq!(json["config"]["server"]["auth_token_file"], "***");
    assert_eq!(
        json["config"]["database"]["url"],
        "postgres://kevin:***@db.example:5432/kevin"
    );
    insta::assert_snapshot!("show_with_sources_server_profile", with_sources);
}

// ---------------------------------------------------------------------------
// AC 5 — profile `kohral` flips exactly the documented defaults
// ---------------------------------------------------------------------------

#[test]
fn ac_ws02_5_kohral_profile_flips_only_documented_defaults() {
    let laptop = load(LoadOptions::hermetic()).unwrap();
    let kohral = load(LoadOptions::hermetic().set("kevin.profile=kohral")).unwrap();

    let expected: BTreeSet<String> = [
        "kevin.profile",
        "kevin.auto_approve_plans",
        "database.auto_migrate",
        "server.docs",
        "server.bind",
        "telemetry.metrics_bind",
        "kohral.enabled",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(diff_keys(&laptop, &kohral), expected);

    let c = &kohral.config;
    assert_eq!(c.kevin.profile, Profile::Kohral);
    assert!(c.kevin.auto_approve_plans);
    assert!(!c.database.auto_migrate);
    assert!(!c.server.docs);
    assert_eq!(c.server.bind.to_string(), "0.0.0.0:7777");
    assert_eq!(c.telemetry.metrics_bind, "0.0.0.0:9464");
    assert!(c.kohral.enabled);
    assert!(!c.workers.fake.enabled, "fake stays off unless set");
    assert_eq!(c.telemetry.log_format, kevin_config::LogFormat::Json);
    assert_eq!(
        c.sandbox.tier,
        SandboxTier::CliNative,
        "profile never touches the sandbox"
    );

    // Provenance: flipped keys come from the profile, not from a file.
    for key in [
        "database.auto_migrate",
        "server.docs",
        "server.bind",
        "kohral.enabled",
        "telemetry.log_format",
    ] {
        assert_eq!(
            kohral.source_of(key),
            Source::Profile(Profile::Kohral),
            "{key}"
        );
    }
    assert_eq!(kohral.source_of("kevin.profile"), Source::Set);

    // Profile values are defaults only: an explicit value anywhere wins.
    let explicit = load(
        LoadOptions::hermetic()
            .set("kevin.profile=kohral")
            .set("database.auto_migrate=true")
            .env("KEVIN__SERVER__BIND", "127.0.0.1:9999"),
    )
    .unwrap();
    assert!(explicit.config.database.auto_migrate);
    assert_eq!(explicit.source_of("database.auto_migrate"), Source::Set);
    assert_eq!(explicit.config.server.bind.to_string(), "127.0.0.1:9999");
    assert_eq!(
        explicit.source_of("server.bind"),
        Source::Env("KEVIN__SERVER__BIND".into())
    );

    insta::assert_snapshot!("kohral_profile_show", kohral.redacted_toml_with_sources());
}

#[test]
fn ac_ws02_5_server_profile_flips_only_its_three_defaults() {
    let laptop = load(LoadOptions::hermetic()).unwrap();
    let server = load(LoadOptions::hermetic().set("kevin.profile=server")).unwrap();
    let expected: BTreeSet<String> = ["kevin.profile", "database.auto_migrate", "server.docs"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(diff_keys(&laptop, &server), expected);
    assert_eq!(
        server.source_of("telemetry.log_format"),
        Source::Profile(Profile::Server)
    );
    assert!(!server.config.kohral.enabled);
    assert_eq!(server.config.server.bind.to_string(), "127.0.0.1:7777");
    insta::assert_snapshot!("server_profile_show", server.redacted_toml());
}
