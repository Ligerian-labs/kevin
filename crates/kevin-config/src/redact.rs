//! Redaction for `kevin config show` / `GET /api/v1/config`
//! (`plan/09-security.md` §Environment and secrets).
//!
//! Key-based: any leaf whose name ends in `token`, `token_file`, `key`,
//! `key_file`, `identity_file` or contains `secret` / `password` is replaced by
//! `***` (counters like `max_tokens_per_task` or `token_grace` are not
//! secrets). Value-based: any string that looks like a URL with a password
//! (`scheme://user:pass@host`) has the password masked. Everything else is
//! untouched.

use std::fmt::Write as _;

use toml::{Table, Value};

use crate::Sources;

/// Replacement for redacted values.
pub const REDACTED: &str = "***";

const SECRET_KEY_SUFFIXES: &[&str] = &["token", "token_file", "key", "key_file", "identity_file"];
const SECRET_KEY_MARKERS: &[&str] = &["secret", "password"];

/// `true` when a leaf key name denotes a secret (or a secret file path).
#[must_use]
pub fn is_secret_key(leaf: &str) -> bool {
    let lower = leaf.to_ascii_lowercase();
    SECRET_KEY_SUFFIXES.iter().any(|s| lower.ends_with(s))
        || SECRET_KEY_MARKERS.iter().any(|m| lower.contains(m))
}

/// Masks the password of `scheme://user:password@host…`; `None` when there is
/// no password to mask.
#[must_use]
pub fn mask_url_password(s: &str) -> Option<String> {
    let (scheme, rest) = s.split_once("://")?;
    let at = rest.find('@')?;
    let (userinfo, host) = rest.split_at(at);
    let (user, _password) = userinfo.split_once(':')?;
    if user.contains('/') || userinfo.contains('/') {
        return None;
    }
    Some(format!("{scheme}://{user}:{REDACTED}{host}"))
}

/// Redacts a string value (URL passwords) in place; `true` when changed.
fn redact_value(value: &mut Value) -> bool {
    match value {
        Value::String(s) => {
            if let Some(masked) = mask_url_password(s) {
                *s = masked;
                return true;
            }
            false
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= redact_value(item);
            }
            changed
        }
        _ => false,
    }
}

/// Redacts `table` in place and returns the dotted paths that were changed.
pub fn redact_table(table: &mut Table) -> Vec<String> {
    let mut changed = Vec::new();
    redact_into(table, "", &mut changed);
    changed
}

fn redact_into(table: &mut Table, prefix: &str, changed: &mut Vec<String>) {
    for (key, value) in table.iter_mut() {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(t) => redact_into(t, &path, changed),
            _ if is_secret_key(key) => {
                // Empty secrets stay visibly empty ("" tells the operator nothing is set).
                let is_empty = matches!(value, Value::String(s) if s.is_empty());
                if !is_empty {
                    *value = Value::String(REDACTED.into());
                    changed.push(path);
                }
            }
            _ => {
                if redact_value(value) {
                    changed.push(path);
                }
            }
        }
    }
}

/// Renders `table` as one `key.path = value  # source` line per leaf, in table
/// order — the `kevin config show --sources` format.
#[must_use]
pub fn render_with_sources(table: &Table, sources: &Sources) -> String {
    let mut out = String::new();
    render_lines(table, "", sources, &mut out);
    out
}

fn render_lines(table: &Table, prefix: &str, sources: &Sources, out: &mut String) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            Value::Table(t) if !t.is_empty() => render_lines(t, &path, sources, out),
            _ => {
                let source = sources
                    .get(&path)
                    .map_or_else(|| "unknown".to_owned(), ToString::to_string);
                let line = format!("{path} = {value}");
                let _ = writeln!(out, "{line:<56} # {source}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_keys() {
        for k in [
            "auth_token_file",
            "token_file",
            "api_key",
            "identity_file",
            "password",
            "client_secret",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in [
            "url",
            "bind",
            "data_dir",
            "env_passthrough",
            "extra_args",
            "token_grace",
            "max_tokens_per_task",
            "context_tokens",
            "context_max_tokens",
        ] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn url_password_masking() {
        assert_eq!(
            mask_url_password("postgres://kevin:s3cr3t@localhost:5432/kevin").as_deref(),
            Some("postgres://kevin:***@localhost:5432/kevin")
        );
        assert_eq!(mask_url_password("postgres://localhost/kevin"), None);
        assert_eq!(mask_url_password("postgres://kevin@localhost/kevin"), None);
        assert_eq!(mask_url_password("not a url"), None);
    }

    #[test]
    fn redacts_table_leaves() {
        let mut t: Table = r#"
[database]
url = "postgres://u:p@h/d"
[server]
auth_token_file = "~/.config/kevin/token"
bind = "127.0.0.1:1"
[kohral]
identity_file = ""
"#
        .parse()
        .unwrap();
        let changed = redact_table(&mut t);
        assert_eq!(changed, vec!["database.url", "server.auth_token_file"]);
        assert_eq!(t["database"]["url"].as_str(), Some("postgres://u:***@h/d"));
        assert_eq!(t["server"]["auth_token_file"].as_str(), Some("***"));
        assert_eq!(t["server"]["bind"].as_str(), Some("127.0.0.1:1"));
        assert_eq!(t["kohral"]["identity_file"].as_str(), Some(""));
    }
}
