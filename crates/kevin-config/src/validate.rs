//! Whole-config validation (`plan/03-config-schema.md` §Validation rules).
//! Every rule pushes into the shared [`ConfigErrors`] so all problems are
//! reported together, each with its key path and the layer that set it.

use kevin_domain::{ModelAlias, WorkerKind};

use crate::Sources;
use crate::error::{ConfigError, ConfigErrors};
use crate::schema::{
    ClaudePermissionMode, CodexSandbox, Embedder, KNOWN_EMBEDDING_DIMENSIONS, KevinConfig, Profile,
    SandboxNetwork, SandboxTier,
};
use crate::source::Source;

/// Worker argv tokens that are only allowed in the `container` tier when they
/// appear in `workers.<kind>.extra_args` (config-level mirror of the policy
/// `kevin-workspace` enforces on the final argv).
pub const FORBIDDEN_EXTRA_ARGS: &[(WorkerKind, &str)] = &[
    (WorkerKind::Claude, "--dangerously-skip-permissions"),
    (WorkerKind::Claude, "bypassPermissions"),
    (WorkerKind::Codex, "danger-full-access"),
    (
        WorkerKind::Codex,
        "--dangerously-bypass-approvals-and-sandbox",
    ),
    (WorkerKind::Codex, "--dangerously-bypass-hook-trust"),
    (WorkerKind::Opencode, "--auto"),
];

/// Source of `key`; an indexed key (`a.b[2]`) falls back to its list (`a.b`).
fn source_of(sources: &Sources, key: &str) -> Source {
    if let Some(source) = sources.get(key) {
        return source.clone();
    }
    key.rsplit_once('[')
        .and_then(|(list, _)| sources.get(list))
        .cloned()
        .unwrap_or(Source::Unknown)
}

/// Runs every rule against `config`, appending to `errors`.
pub fn validate(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    database(config, sources, errors);
    aliases(config, sources, errors);
    models(config, sources, errors);
    sandbox(config, sources, errors);
    ranges(config, sources, errors);
    memory(config, sources, errors);
    insecure_bind(config, sources, errors);
}

fn database(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    let db = &config.database;
    let url_source = source_of(sources, "database.url");
    let url_explicit = url_source != Source::Default && !db.url.is_empty();
    match (&db.url_file, url_explicit, db.url.is_empty()) {
        // url_file set together with an explicitly configured url → ambiguous.
        (Some(_), true, _) => errors.push(ConfigError::DatabaseUrlExactlyOne {
            layer: source_of(sources, "database.url_file"),
        }),
        // Nothing configured at all.
        (None, _, true) => errors.push(ConfigError::DatabaseUrlExactlyOne { layer: url_source }),
        // url_file wins over the default url, or url is the only one: check the url when used.
        (Some(_), false, _) => {}
        (None, _, false) => {
            if let Err(message) = check_postgres_url(&db.url) {
                errors.push(ConfigError::InvalidDatabaseUrl {
                    layer: url_source,
                    message,
                });
            }
        }
    }
}

/// `postgres://` / `postgresql://` with a host.
fn check_postgres_url(raw: &str) -> Result<(), String> {
    let parsed = url::Url::parse(raw).map_err(|e| format!("not a valid URL: {e}"))?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") {
        return Err(format!(
            "scheme {:?} is not postgres:// or postgresql://",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("missing host".into());
    }
    Ok(())
}

fn check_alias_ref(
    config: &KevinConfig,
    key: String,
    alias: &ModelAlias,
    source: Source,
    errors: &mut ConfigErrors,
) {
    match config.models.get(alias) {
        None => errors.push(ConfigError::UnknownModelAlias {
            key,
            layer: source,
            alias: alias.clone(),
        }),
        Some(entry) if !config.workers.is_enabled(entry.worker) => {
            errors.push(ConfigError::ModelWorkerDisabled {
                key,
                layer: source,
                alias: alias.clone(),
                worker: entry.worker,
            });
        }
        Some(_) => {}
    }
}

fn aliases(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    for (key, alias) in config.roles.bindings() {
        check_alias_ref(
            config,
            key.to_owned(),
            alias,
            source_of(sources, key),
            errors,
        );
    }
    for (kind, routing) in &config.routing.kinds {
        let list_key = format!("routing.kinds.{kind}.candidates");
        let source = source_of(sources, &list_key);
        for (i, alias) in routing.candidates.iter().enumerate() {
            check_alias_ref(
                config,
                format!("{list_key}[{i}]"),
                alias,
                source.clone(),
                errors,
            );
        }
    }
}

fn models(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    for (alias, entry) in &config.models {
        if entry.worker == WorkerKind::Pi && entry.provider().is_none_or(str::is_empty) {
            errors.push(ConfigError::InvalidModelEntry {
                alias: alias.clone(),
                layer: source_of(sources, &format!("models.{alias}.worker")),
                message: "pi aliases require a `provider` key (e.g. provider = \"anthropic\")"
                    .into(),
            });
        }
        if entry.model.is_empty() {
            errors.push(ConfigError::InvalidModelEntry {
                alias: alias.clone(),
                layer: source_of(sources, &format!("models.{alias}.model")),
                message: "`model` must not be empty".into(),
            });
        }
    }
}

fn sandbox(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    let tier = config.sandbox.tier;
    if tier == SandboxTier::Container {
        return;
    }
    let tier_name = tier.to_string();
    let mut forbidden = |key: &str, value: String| {
        errors.push(ConfigError::ForbiddenOutsideContainer {
            key: key.to_owned(),
            layer: source_of(sources, key),
            value,
            tier: tier_name.clone(),
        });
    };
    if config.sandbox.allow_dangerous_flags {
        forbidden("sandbox.allow_dangerous_flags", "true".into());
    }
    if config.sandbox.network == SandboxNetwork::Deny {
        forbidden("sandbox.network", "deny".into());
    }
    if config.workers.claude.permission_mode == ClaudePermissionMode::BypassPermissions {
        forbidden("workers.claude.permission_mode", "bypassPermissions".into());
    }
    if config.workers.codex.sandbox == CodexSandbox::DangerFullAccess {
        forbidden("workers.codex.sandbox", "danger-full-access".into());
    }
    let extra_args = [
        (WorkerKind::Claude, &config.workers.claude.extra_args),
        (WorkerKind::Codex, &config.workers.codex.extra_args),
        (WorkerKind::Pi, &config.workers.pi.extra_args),
        (WorkerKind::Opencode, &config.workers.opencode.extra_args),
    ];
    for (kind, args) in extra_args {
        for (i, arg) in args.iter().enumerate() {
            let hit = FORBIDDEN_EXTRA_ARGS
                .iter()
                .any(|(k, flag)| *k == kind && arg.split([' ', '=']).any(|token| token == *flag));
            if hit {
                forbidden(&format!("workers.{kind}.extra_args[{i}]"), arg.clone());
            }
        }
    }
}

fn ranges(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    let mut out_of_range = |key: &str, message: &str| {
        errors.push(ConfigError::OutOfRange {
            key: key.to_owned(),
            layer: source_of(sources, key),
            message: message.to_owned(),
        });
    };
    let b = &config.budget;
    if b.default_run_usd <= rust_decimal::Decimal::ZERO {
        out_of_range("budget.default_run_usd", "must be > 0");
    }
    if b.default_task_usd <= rust_decimal::Decimal::ZERO {
        out_of_range("budget.default_task_usd", "must be > 0");
    }
    if b.default_run_wall.is_zero() {
        out_of_range("budget.default_run_wall", "must be > 0");
    }
    if b.default_task_wall.is_zero() {
        out_of_range("budget.default_task_wall", "must be > 0");
    }
    if b.max_attempts == 0 {
        out_of_range("budget.max_attempts", "must be >= 1");
    }
    if b.max_parallel_tasks == 0 {
        out_of_range("budget.max_parallel_tasks", "must be >= 1");
    }
    if b.max_tokens_per_task == 0 {
        out_of_range("budget.max_tokens_per_task", "must be > 0");
    }
    if config.database.pool_size == 0 {
        out_of_range("database.pool_size", "must be >= 1");
    }
    let r = &config.routing;
    for (key, value) in [
        ("routing.exploration", r.exploration),
        ("routing.quality_weight", r.quality_weight),
        ("routing.cost_weight", r.cost_weight),
        ("routing.latency_weight", r.latency_weight),
        (
            "orchestrator.question_confidence_threshold",
            config.orchestrator.question_confidence_threshold,
        ),
        ("memory.min_similarity", config.memory.min_similarity),
    ] {
        if !(0.0..=1.0).contains(&value) || value.is_nan() {
            out_of_range(key, "must be between 0 and 1");
        }
    }
    for (key, value) in [
        (
            "concurrency.blocking_threads",
            config.concurrency.blocking_threads,
        ),
        ("memory.top_k", config.memory.top_k),
        ("memory.dimensions", config.memory.dimensions),
    ] {
        if value == 0 {
            out_of_range(key, "must be >= 1");
        }
    }
    for (kind, limit) in &config.concurrency.per_worker_kind {
        if *limit == 0 {
            out_of_range(
                &format!("concurrency.per_worker_kind.{kind}"),
                "must be >= 1",
            );
        }
    }
}

fn memory(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    let m = &config.memory;
    if m.embedder != Embedder::Fastembed {
        return;
    }
    if let Some((_, expected)) = KNOWN_EMBEDDING_DIMENSIONS
        .iter()
        .find(|(model, _)| *model == m.embedding_model)
        && *expected != m.dimensions
    {
        errors.push(ConfigError::EmbeddingDimensionMismatch {
            layer: source_of(sources, "memory.dimensions"),
            model: m.embedding_model.clone(),
            expected: *expected,
            actual: m.dimensions,
        });
    }
}

fn insecure_bind(config: &KevinConfig, sources: &Sources, errors: &mut ConfigErrors) {
    if config.server.bind.ip().is_loopback() {
        return;
    }
    let has_api_token = !config.server.auth_token_file.as_os_str().is_empty();
    let has_kohral_token = (config.kevin.profile == Profile::Kohral || config.kohral.enabled)
        && !config.kohral.token_file.as_os_str().is_empty();
    // Whether the file *exists with mode 0600* cannot be decided here:
    // `load()` is a pure function of the layers (a config may legitimately be
    // validated on a machine that will never serve it), and the file may be
    // created between `load` and `serve`. `crate::token::check_bind_security`
    // makes that check at startup, which is what `plan/09-security.md`
    // §API authentication asks for.
    if !has_api_token && !has_kohral_token {
        errors.push(ConfigError::InsecureBind {
            bind: config.server.bind.to_string(),
            layer: source_of(sources, "server.bind"),
            reason: "no token file is configured".to_owned(),
        });
    }
}
