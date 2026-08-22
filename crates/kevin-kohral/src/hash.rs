//! Canonical request hashing (`plan/08-kohral-runtime.md` §1.2).
//!
//! Kohral retries a turn with the same `Idempotency-Key`; the runtime must
//! decide "same request" the same way Hermes does, or an innocent retry looks
//! like a `409 idempotency_conflict`. Hermes'
//! `kohral_run_store.canonical_request_hash` is
//!
//! ```python
//! json.dumps({"body": body, "session_key": session_key},
//!            ensure_ascii=False, allow_nan=False, sort_keys=True,
//!            separators=(",", ":"))
//! ```
//!
//! hashed with SHA-256. [`canonical_json`] reproduces that byte-for-byte:
//! object keys sorted by Unicode code point (which is what `BTreeMap` over
//! UTF-8 gives), no separator whitespace, non-ASCII emitted verbatim, the same
//! short escapes (`\b \f \n \r \t \" \\`) and `\u00xx` for the remaining
//! control characters. It is written by hand rather than delegated to
//! `serde_json::to_string` so that a `preserve_order` feature enabled anywhere
//! in the dependency graph cannot silently change the key order — and with it
//! every hash Kevin has already stored.

use std::fmt::Write as _;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// `sha256(json_canonical({"body": body, "session_key": session_key}))`.
///
/// Byte-compatible with `kohral_run_store.canonical_request_hash`.
#[must_use]
pub fn canonical_request_hash(body: &Value, session_key: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_request_json(body, session_key).as_bytes());
    hex(&hasher.finalize())
}

/// The canonical envelope Kohral hashes, as a string (stored in
/// `kohral.runs_ledger.request_json`).
#[must_use]
pub fn canonical_request_json(body: &Value, session_key: Option<&str>) -> String {
    let mut envelope = Map::new();
    envelope.insert("body".to_owned(), body.clone());
    envelope.insert(
        "session_key".to_owned(),
        session_key.map_or(Value::Null, |key| Value::String(key.to_owned())),
    );
    canonical_json(&Value::Object(envelope))
}

/// Python's `json.dumps(value, ensure_ascii=False, allow_nan=False,
/// sort_keys=True, separators=(",", ":"))`.
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            // `serde_json::Map` is a `BTreeMap` unless `preserve_order` is on;
            // sorting here makes the order independent of that feature.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, &map[key]);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, number: &serde_json::Number) {
    // Integers print identically in both languages. Floats go through Rust's
    // shortest round-trip formatting, which agrees with Python's `repr` for
    // every finite `f64`; `allow_nan=False` has no counterpart to break
    // because `serde_json` cannot hold NaN or ±Inf in the first place.
    if let Some(value) = number.as_f64()
        && number.as_i64().is_none()
        && number.as_u64().is_none()
    {
        #[allow(clippy::float_cmp)]
        let integral = value == value.trunc();
        if integral && value.abs() < 1e16 {
            let _ = write!(out, "{value:.1}");
        } else {
            let _ = write!(out, "{value}");
        }
        return;
    }
    out.push_str(&number.to_string());
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{canonical_json, canonical_request_hash, canonical_request_json};

    #[test]
    fn keys_are_sorted_and_separators_are_tight() {
        let value = json!({"b": 1, "a": [1, 2], "A": null});
        assert_eq!(canonical_json(&value), r#"{"A":null,"a":[1,2],"b":1}"#);
    }

    #[test]
    fn non_ascii_is_emitted_verbatim_and_controls_are_escaped() {
        let value = json!({"k": "é\u{1}\n\"\\"});
        assert_eq!(canonical_json(&value), "{\"k\":\"é\\u0001\\n\\\"\\\\\"}");
    }

    #[test]
    fn the_envelope_matches_the_hermes_shape() {
        let body = json!({"input": "hi", "model": "hermes-agent"});
        assert_eq!(
            canonical_request_json(&body, Some("kohral:c1")),
            r#"{"body":{"input":"hi","model":"hermes-agent"},"session_key":"kohral:c1"}"#
        );
        assert_eq!(
            canonical_request_json(&body, None),
            r#"{"body":{"input":"hi","model":"hermes-agent"},"session_key":null}"#
        );
    }

    #[test]
    fn the_hash_ignores_key_order_but_not_content() {
        let a = json!({"input": "hi", "model": "m"});
        let b = json!({"model": "m", "input": "hi"});
        assert_eq!(
            canonical_request_hash(&a, Some("k")),
            canonical_request_hash(&b, Some("k"))
        );
        assert_ne!(
            canonical_request_hash(&a, Some("k")),
            canonical_request_hash(&a, Some("other"))
        );
        assert_ne!(
            canonical_request_hash(&a, Some("k")),
            canonical_request_hash(&json!({"input": "ho"}), Some("k"))
        );
    }

    /// Pinned against a value computed with `CPython`'s `json.dumps` +
    /// `hashlib.sha256`, so a refactor cannot drift away from Hermes.
    #[test]
    fn the_hash_is_pinned_to_the_python_implementation() {
        let body = serde_json::json!({
            "input": "reply deterministically",
            "instructions": "",
            "conversation_history": [],
            "session_id": "conformance",
            "model": "hermes-agent",
        });
        assert_eq!(
            canonical_request_json(&body, Some("kohral:conformance")),
            "{\"body\":{\"conversation_history\":[],\"input\":\"reply deterministically\",\
             \"instructions\":\"\",\"model\":\"hermes-agent\",\"session_id\":\"conformance\"},\
             \"session_key\":\"kohral:conformance\"}"
        );
        assert_eq!(
            canonical_request_hash(&body, Some("kohral:conformance")),
            "916c4a5ff475bb467e513c794f34887adca9dc834c2c0559d5c901c91baada64"
        );
    }
}
