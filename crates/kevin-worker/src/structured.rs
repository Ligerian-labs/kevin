//! Structured output extraction and validation (`plan/04-workers.md`
//! §Structured output).
//!
//! 1. Take the CLI's native structured output when it returned one; otherwise
//!    extract the first balanced JSON object/array from the final text — inside
//!    a ```` ```json ```` fence or bare, surrounded by prose.
//! 2. Light repairs: strip fences, drop trailing commas.
//! 3. Validate with the `jsonschema` crate; on failure the caller may run one
//!    repair turn using [`repair_prompt`], then fail with
//!    `Failed{Permanent, "schema_violation"}`.

use serde_json::Value;

/// Why structured output could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StructuredError {
    /// No JSON object/array found in the text.
    #[error("no JSON object or array found in the answer")]
    NotFound,
    /// JSON found but it does not parse even after repairs.
    #[error("answer contains malformed JSON: {message}")]
    Parse {
        /// Parser message.
        message: String,
    },
    /// The schema itself is invalid.
    #[error("invalid output schema: {message}")]
    InvalidSchema {
        /// Compiler message.
        message: String,
    },
    /// The JSON does not match the schema.
    #[error("schema_violation: {}", errors.join("; "))]
    SchemaViolation {
        /// One line per violation (`<instance path>: <message>`).
        errors: Vec<String>,
    },
}

impl StructuredError {
    /// `true` for [`StructuredError::SchemaViolation`].
    #[must_use]
    pub const fn is_schema_violation(&self) -> bool {
        matches!(self, StructuredError::SchemaViolation { .. })
    }
}

/// Extracts the first JSON object/array from `text` and validates it against
/// `schema`. Fenced blocks are preferred over bare JSON.
pub fn extract_and_validate(text: &str, schema: &Value) -> Result<Value, StructuredError> {
    let value = extract(text)?;
    validate(&value, schema)?;
    Ok(value)
}

/// Validates `value` against `schema`.
pub fn validate(value: &Value, schema: &Value) -> Result<(), StructuredError> {
    let validator =
        jsonschema::validator_for(schema).map_err(|err| StructuredError::InvalidSchema {
            message: err.to_string(),
        })?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|err| {
            let path = err.instance_path().to_string();
            if path.is_empty() {
                format!("/: {err}")
            } else {
                format!("{path}: {err}")
            }
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(StructuredError::SchemaViolation { errors })
    }
}

/// Extracts (and lightly repairs) the first JSON object/array in `text`.
pub fn extract(text: &str) -> Result<Value, StructuredError> {
    let mut last_parse_error = None;
    for candidate in candidates(text) {
        match parse_with_repairs(candidate) {
            Ok(value) => return Ok(value),
            Err(message) => last_parse_error = Some(message),
        }
    }
    match last_parse_error {
        Some(message) => Err(StructuredError::Parse { message }),
        None => Err(StructuredError::NotFound),
    }
}

/// The follow-up prompt for the single repair turn.
#[must_use]
pub fn repair_prompt(err: &StructuredError) -> String {
    format!("Your previous answer did not match the schema: {err}. Reply with only corrected JSON.")
}

/// Candidate JSON snippets, fenced blocks first, then balanced bare spans.
fn candidates(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for fenced in fenced_blocks(text) {
        if let Some(span) = first_balanced(fenced) {
            out.push(span);
        }
    }
    let mut rest = text;
    while let Some(span) = first_balanced(rest) {
        out.push(span);
        let start = span.as_ptr() as usize - rest.as_ptr() as usize;
        rest = &rest[start + span.len()..];
    }
    out
}

/// Contents of every ```` ``` ```` fence (any language tag).
fn fenced_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        let body_start = after_open.find('\n').map_or(after_open.len(), |i| i + 1);
        let body = &after_open[body_start..];
        let Some(close) = body.find("```") else {
            break;
        };
        blocks.push(&body[..close]);
        rest = &body[close + 3..];
    }
    blocks
}

/// First balanced `{…}` / `[…]` span in `text`, string-aware.
fn first_balanced(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    // Unbalanced: return the tail so the parse error surfaces.
    Some(&text[start..])
}

fn parse_with_repairs(candidate: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(candidate) {
        Ok(v) => Ok(v),
        Err(first) => {
            let repaired = strip_trailing_commas(candidate);
            serde_json::from_str::<Value>(&repaired).map_err(|_| first.to_string())
        }
    }
}

/// Removes `,` directly before `}` / `]` (outside strings).
fn strip_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut pending_comma: Option<String> = None;
    for c in text.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                flush(&mut out, &mut pending_comma);
                in_string = true;
                out.push(c);
            }
            ',' => {
                flush(&mut out, &mut pending_comma);
                pending_comma = Some(String::from(","));
            }
            '}' | ']' => {
                pending_comma = None;
                out.push(c);
            }
            c if c.is_whitespace() => {
                if let Some(p) = pending_comma.as_mut() {
                    p.push(c);
                } else {
                    out.push(c);
                }
            }
            _ => {
                flush(&mut out, &mut pending_comma);
                out.push(c);
            }
        }
    }
    flush(&mut out, &mut pending_comma);
    out
}

fn flush(out: &mut String, pending: &mut Option<String>) {
    if let Some(p) = pending.take() {
        out.push_str(&p);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["status"],
            "properties": { "status": { "type": "string", "enum": ["ok", "error"] } },
            "additionalProperties": false
        })
    }

    #[test]
    fn extracts_fenced_json_with_prose_around() {
        let text = "Sure! Here is the result:\n```json\n{\"status\": \"ok\"}\n```\nDone.";
        assert_eq!(
            extract_and_validate(text, &schema()).unwrap(),
            json!({"status": "ok"})
        );
    }

    #[test]
    fn extracts_bare_json_and_repairs_trailing_commas() {
        let text = "result: {\"status\": \"ok\",} trailing";
        assert_eq!(extract(text).unwrap(), json!({"status": "ok"}));
        let arr = "[1, 2, 3,]";
        assert_eq!(extract(arr).unwrap(), json!([1, 2, 3]));
        let nested = "x {\"a\": {\"b\": [1, {\"c\": \"}\"}]}} y";
        assert_eq!(
            extract(nested).unwrap(),
            json!({"a": {"b": [1, {"c": "}"}]}})
        );
    }

    #[test]
    fn prefers_fenced_block_over_earlier_bare_json() {
        let text = "ignore {\"status\": \"error\"}\n```\n{\"status\": \"ok\"}\n```";
        assert_eq!(extract(text).unwrap(), json!({"status": "ok"}));
    }

    #[test]
    fn rejects_schema_violations_with_paths() {
        let err = extract_and_validate("{\"status\": \"maybe\"}", &schema()).unwrap_err();
        assert!(err.is_schema_violation());
        let text = err.to_string();
        assert!(text.starts_with("schema_violation"), "{text}");
        assert!(text.contains("/status"), "{text}");
        let err = extract_and_validate("{\"other\": 1}", &schema()).unwrap_err();
        assert!(err.is_schema_violation());
        assert!(repair_prompt(&err).contains("did not match the schema"));
    }

    #[test]
    fn reports_not_found_parse_and_invalid_schema() {
        assert_eq!(extract("no json here"), Err(StructuredError::NotFound));
        assert!(matches!(
            extract("{\"a\": }"),
            Err(StructuredError::Parse { .. })
        ));
        assert!(matches!(
            extract_and_validate("{}", &json!({"type": 12})),
            Err(StructuredError::InvalidSchema { .. })
        ));
    }
}
