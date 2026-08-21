//! Redaction (`plan/09-security.md` §Redaction, `plan/10` §Logging).
//!
//! One [`Redactor`] is applied to every sink: tracing records (through the
//! layer in [`crate::layer`]), task logs before persistence, event payloads
//! before append, memory content before embedding, API error messages and SSE
//! payloads. It is allow-list by structure (callers only emit known fields)
//! and deny-list by pattern (this module).
//!
//! Every pattern replaces its match with `[REDACTED:<kind>]`; every new secret
//! kind adds a corpus line to `tests/redact_corpus.txt`.

use std::borrow::Cow;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::{Arc, OnceLock, RwLock};

use regex::Regex;
use serde_json::Value;

/// Marker used for redacted spans of text: `[REDACTED:<kind>]`.
pub const MARKER_PREFIX: &str = "[REDACTED:";

/// Upper bound on one logged/persisted field value (8 KiB).
pub const FIELD_CAP_BYTES: usize = 8 * 1024;
/// Upper bound on a stack trace / error chain (8 KiB).
pub const STACK_TRACE_CAP_BYTES: usize = 8 * 1024;
/// Upper bound on one log record (32 KiB).
pub const RECORD_CAP_BYTES: usize = 32 * 1024;
/// Upper bound on one transcript line before persistence (64 KiB).
pub const TRANSCRIPT_LINE_CAP_BYTES: usize = 64 * 1024;

/// A redaction pattern: a regex and the kind written in the marker.
#[derive(Debug, Clone)]
pub struct Pattern {
    /// Short machine name of the secret kind (`anthropic_key`, `bearer`, …).
    pub kind: &'static str,
    /// Compiled regex.
    pub regex: Regex,
    /// Which capture group holds the secret (0 = whole match). Groups let a
    /// pattern keep a visible prefix (`Bearer `, `KEY=`) and blank the value.
    pub group: usize,
}

/// The built-in deny-list, in application order.
///
/// Kinds: `private_key`, `anthropic_key`, `openai_key`, `github_token`,
/// `aws_key`, `slack_token`, `jwt`, `bearer`, `postgres_password`,
/// `env_secret`, plus `secret` for runtime-registered values.
pub fn builtin_patterns() -> Vec<Pattern> {
    fn p(kind: &'static str, re: &str, group: usize) -> Pattern {
        Pattern {
            kind,
            regex: Regex::new(re).unwrap_or_else(|e| panic!("builtin redaction regex {kind}: {e}")),
            group,
        }
    }
    vec![
        // PEM private key blocks (multi-line).
        p(
            "private_key",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            0,
        ),
        // Anthropic keys must come before the generic OpenAI `sk-` shape.
        p("anthropic_key", r"sk-ant-[A-Za-z0-9_\-]{8,}", 0),
        p(
            "openai_key",
            r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_\-]{8,}",
            0,
        ),
        p(
            "github_token",
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|\bgithub_pat_[A-Za-z0-9_]{20,}",
            0,
        ),
        p("aws_key", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b", 0),
        p("slack_token", r"\bxox[abpr]-[A-Za-z0-9\-]{10,}", 0),
        // JWT: three base64url segments, the first two starting with `eyJ`.
        p(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]+\.eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
            0,
        ),
        // `Authorization: Bearer <token>` (keeps the `Bearer ` prefix).
        p("bearer", r"(?i)\bbearer\s+([A-Za-z0-9_\-\.=+/]{8,})", 1),
        // `postgres://user:password@host` (keeps user and host).
        p(
            "postgres_password",
            r"(?i)\bpostgres(?:ql)?://[^:/\s@]+:([^@\s]+)@",
            1,
        ),
        // `FOO_KEY=value`, `SOME_TOKEN=value`, `X_SECRET: value`, `password=value`.
        p(
            "env_secret",
            r#"(?i)\b(?:[A-Z0-9_]*(?:_KEY|_TOKEN|_SECRET|PASSWORD|PASSWD|API_KEY|ACCESS_KEY)[A-Z0-9_]*)\s*[=:]\s*["']?([^\s"',;&]{4,})"#,
            1,
        ),
    ]
}

/// Redacts secrets from text and JSON values.
///
/// Cheap to clone (`Arc` inside). Runtime secrets registered with
/// [`Redactor::register_secret`] are matched by hash and length, never kept in
/// clear.
#[derive(Debug, Clone)]
pub struct Redactor {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    patterns: Vec<Pattern>,
    hasher: RandomState,
    secrets: RwLock<SecretSet>,
}

#[derive(Debug, Default)]
struct SecretSet {
    /// `(len, hash)` of every registered secret.
    hashes: HashSet<(usize, u64)>,
    /// Distinct lengths, so the scan only hashes windows of those sizes.
    lengths: Vec<usize>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new(builtin_patterns())
    }
}

impl Redactor {
    /// A redactor with the given patterns (see [`builtin_patterns`]).
    #[must_use]
    pub fn new(patterns: Vec<Pattern>) -> Self {
        Self {
            inner: Arc::new(Inner {
                patterns,
                hasher: RandomState::new(),
                secrets: RwLock::new(SecretSet::default()),
            }),
        }
    }

    /// The process-wide redactor used by [`crate::init`] and available to
    /// other crates (store, API, memory) without plumbing.
    pub fn global() -> &'static Redactor {
        static GLOBAL: OnceLock<Redactor> = OnceLock::new();
        GLOBAL.get_or_init(Redactor::default)
    }

    /// Registers the exact runtime value of a secret (API token, db password,
    /// Kohral runtime token…) so that it is masked wherever it appears. Values
    /// shorter than 4 bytes are ignored (they would mask everything).
    pub fn register_secret(&self, secret: &str) {
        if secret.len() < 4 {
            return;
        }
        let hash = self.hash(secret.as_bytes());
        let mut set = self.secrets();
        set.hashes.insert((secret.len(), hash));
        if !set.lengths.contains(&secret.len()) {
            set.lengths.push(secret.len());
            set.lengths.sort_unstable_by(|a, b| b.cmp(a));
        }
    }

    /// Redacts `text`. Returns `Cow::Borrowed` when nothing matched.
    pub fn redact_str<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for pattern in &self.inner.patterns {
            if let Some(replaced) = apply_pattern(pattern, &out) {
                out = Cow::Owned(replaced);
            }
        }
        if let Some(replaced) = self.apply_secrets(&out) {
            out = Cow::Owned(replaced);
        }
        out
    }

    /// Redacts every string inside a JSON value in place (keys included:
    /// a key such as `"sk-…"` would otherwise leak).
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                if let Cow::Owned(redacted) = self.redact_str(s) {
                    *s = redacted;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.redact_value(v)),
            Value::Object(map) => {
                let needs_key_rewrite = map
                    .keys()
                    .any(|k| matches!(self.redact_str(k), Cow::Owned(_)));
                if needs_key_rewrite {
                    let old = std::mem::take(map);
                    for (k, mut v) in old {
                        self.redact_value(&mut v);
                        map.insert(self.redact_str(&k).into_owned(), v);
                    }
                } else {
                    map.values_mut().for_each(|v| self.redact_value(v));
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// Redacts and caps a free-text field (default field cap).
    pub fn redact_field(&self, text: &str) -> String {
        truncate(&self.redact_str(text), FIELD_CAP_BYTES)
    }

    fn apply_secrets(&self, text: &str) -> Option<String> {
        let set = self.secrets();
        if set.hashes.is_empty() {
            return None;
        }
        let bytes = text.as_bytes();
        let mut out = String::new();
        let mut last = 0usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if !text.is_char_boundary(i) {
                i += 1;
                continue;
            }
            let mut matched = None;
            for &len in &set.lengths {
                let end = i + len;
                if end > bytes.len() || !text.is_char_boundary(end) {
                    continue;
                }
                if set.hashes.contains(&(len, self.hash(&bytes[i..end]))) {
                    matched = Some(end);
                    break;
                }
            }
            if let Some(end) = matched {
                out.push_str(&text[last..i]);
                out.push_str(&marker("secret"));
                last = end;
                i = end;
            } else {
                i += 1;
            }
        }
        if last == 0 {
            return None;
        }
        out.push_str(&text[last..]);
        Some(out)
    }

    fn hash(&self, bytes: &[u8]) -> u64 {
        let mut h = self.inner.hasher.build_hasher();
        h.write(bytes);
        h.finish()
    }

    fn secrets(&self) -> std::sync::RwLockWriteGuard<'_, SecretSet> {
        self.inner
            .secrets
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn apply_pattern(pattern: &Pattern, text: &str) -> Option<String> {
    let mut out = String::new();
    let mut last = 0usize;
    for caps in pattern.regex.captures_iter(text) {
        let Some(m) = caps.get(pattern.group) else {
            continue;
        };
        // Never re-redact an earlier pattern's marker (keeps the specific kind).
        if m.as_str().starts_with(MARKER_PREFIX) {
            continue;
        }
        out.push_str(&text[last..m.start()]);
        out.push_str(&marker(pattern.kind));
        last = m.end();
    }
    if last == 0 {
        return None;
    }
    out.push_str(&text[last..]);
    Some(out)
}

/// `[REDACTED:<kind>]`.
#[must_use]
pub fn marker(kind: &str) -> String {
    format!("{MARKER_PREFIX}{kind}]")
}

/// Whether the text contains at least one redaction marker.
#[must_use]
pub fn contains_marker(text: &str) -> bool {
    text.contains(MARKER_PREFIX)
}

/// Truncates `text` to at most `cap` bytes (on a char boundary), appending
/// `…[truncated N bytes]` when anything was cut.
#[must_use]
pub fn truncate(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = text.len() - end;
    format!("{}…[truncated {dropped} bytes]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_anthropic_and_openai_keys() {
        let r = Redactor::default();
        assert_eq!(
            r.redact_str("key sk-ant-api03-abcdefghijklmnop done"),
            "key [REDACTED:anthropic_key] done"
        );
        assert_eq!(
            r.redact_str("sk-proj-AbCdEfGhIjKlMnOp1234"),
            "[REDACTED:openai_key]"
        );
    }

    #[test]
    fn keeps_bearer_prefix_and_postgres_host() {
        let r = Redactor::default();
        assert_eq!(
            r.redact_str("Authorization: Bearer abcdefgh.ijkl-MNOP"),
            "Authorization: Bearer [REDACTED:bearer]"
        );
        assert_eq!(
            r.redact_str("postgres://kevin:s3cr3t@localhost:5433/kevin"),
            "postgres://kevin:[REDACTED:postgres_password]@localhost:5433/kevin"
        );
    }

    #[test]
    fn registered_secret_is_masked_by_hash() {
        let r = Redactor::default();
        r.register_secret("hunter2-token-value");
        assert_eq!(
            r.redact_str("token is hunter2-token-value ok"),
            "token is [REDACTED:secret] ok"
        );
        assert_eq!(r.redact_str("hunter2"), "hunter2");
        r.register_secret("abc"); // too short, ignored
        assert_eq!(r.redact_str("abc"), "abc");
    }

    #[test]
    fn redacts_json_values_and_keys() {
        let r = Redactor::default();
        let mut v = serde_json::json!({
            "nested": {"OPENAI_API_KEY=sk-abcdefghijklmnop": ["Bearer tokentoken1234"]},
            "n": 1
        });
        r.redact_value(&mut v);
        let text = v.to_string();
        assert!(!text.contains("abcdefghijklmnop"), "{text}");
        assert!(!text.contains("tokentoken1234"), "{text}");
        assert!(text.contains("\"n\":1"));
    }

    #[test]
    fn truncation_marks_dropped_bytes() {
        let t = truncate("héllo wörld", 6);
        assert!(t.starts_with("héllo"));
        assert!(t.ends_with("…[truncated 7 bytes]"), "{t}");
        assert_eq!(truncate("short", 10), "short");
    }
}
