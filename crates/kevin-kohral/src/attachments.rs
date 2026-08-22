//! Temporary attachments (`plan/08-kohral-runtime.md` §1.8).
//!
//! Kohral streams the raw bytes of a file a user attached to a message and
//! expects back `{"path", "size", "sha256"}` with a path **under**
//! `/tmp/kohral-uploads/` — it validates the prefix and refuses anything else.
//! The bytes live on an ephemeral tmpfs and are handed to workers as read-only
//! inputs of the turn ([`crate::turn::TurnRequest::artifacts`]).
//!
//! Security notes, because this is the one endpoint that writes attacker-named
//! files to disk:
//!
//! - the three path segments must be *safe identifiers*; `..`, `/` and `\` can
//!   never appear, so the write cannot escape the upload root;
//! - the filename header is base64url-encoded by Kohral and is sanitised again
//!   here before it is used;
//! - the digest is verified after the write and a mismatch deletes the file;
//! - `kohral.max_attachment_bytes` is enforced by the router's body limit and
//!   re-checked here.

use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{KohralError, KohralErrorCode, KohralResult};
use crate::state::KohralState;

/// Header carrying the base64url-encoded original filename.
pub const FILENAME_HEADER: &str = "x-kohral-filename";
/// Header carrying the expected hex digest.
pub const SHA256_HEADER: &str = "x-kohral-sha256";

/// Longest identifier accepted in a path segment.
const MAX_SEGMENT: usize = 128;
/// Longest sanitised filename kept in the stored name.
const MAX_FILENAME: usize = 100;

/// `PUT /v1/attachments/{conversation_id}/{message_id}/{attachment_id}`.
pub async fn put(
    State(state): State<KohralState>,
    Path((conversation, message, attachment)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match store(
        &state,
        &conversation,
        &message,
        &attachment,
        &headers,
        &body,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

/// `DELETE /v1/attachments/{conversation_id}/{message_id}/{attachment_id}`.
pub async fn delete(
    State(state): State<KohralState>,
    Path((conversation, message, attachment)): Path<(String, String, String)>,
) -> Response {
    let root = state.options().upload_root.clone();
    match directory(&root, &conversation, &message) {
        Ok(dir) => {
            if let Err(error) = remove(&dir, &attachment) {
                tracing::warn!(error = %error, "removing a Kohral attachment failed");
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(error) => error.into_response(),
    }
}

#[allow(clippy::unused_async)]
async fn store(
    state: &KohralState,
    conversation: &str,
    message: &str,
    attachment: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> KohralResult<Response> {
    if !state.options().temporary_attachments {
        return Err(KohralError::new(
            KohralErrorCode::InvalidRequest,
            "this runtime does not accept temporary attachments",
        ));
    }
    let limit = state.options().max_attachment_bytes;
    if u64::try_from(body.len()).unwrap_or(u64::MAX) > limit {
        return Err(KohralError::new(
            KohralErrorCode::AttachmentTooLarge,
            format!("attachments are limited to {limit} bytes"),
        ));
    }
    let id = safe_segment(attachment)?;
    let dir = directory(&state.options().upload_root, conversation, message)?;

    let digest = hex(&Sha256::digest(body));
    if let Some(expected) = headers.get(SHA256_HEADER).and_then(|v| v.to_str().ok())
        && !expected.trim().eq_ignore_ascii_case(&digest)
    {
        return Err(KohralError::new(
            KohralErrorCode::InvalidAttachment,
            "the uploaded bytes do not match X-Kohral-Sha256",
        ));
    }

    let name = filename(headers);
    let path = dir.join(format!("{id}--{name}"));
    std::fs::create_dir_all(&dir).map_err(|error| io_error(&error))?;
    std::fs::write(&path, body).map_err(|error| io_error(&error))?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "path": path.to_string_lossy(),
            "size": body.len(),
            "sha256": digest,
        })),
    )
        .into_response())
}

fn remove(dir: &FsPath, attachment: &str) -> KohralResult<()> {
    let id = safe_segment(attachment)?;
    let prefix = format!("{id}--");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(prefix.as_str())
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn directory(root: &FsPath, conversation: &str, message: &str) -> KohralResult<PathBuf> {
    Ok(root
        .join(safe_segment(conversation)?)
        .join(safe_segment(message)?))
}

/// `[A-Za-z0-9._-]{1,128}`, and never `.` or `..`.
fn safe_segment(value: &str) -> KohralResult<String> {
    let invalid = || {
        KohralError::new(
            KohralErrorCode::InvalidAttachment,
            "attachment path segments must match [A-Za-z0-9._-]{1,128}",
        )
    };
    if value.is_empty() || value.len() > MAX_SEGMENT || value == "." || value == ".." {
        return Err(invalid());
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid());
    }
    Ok(value.to_owned())
}

/// The original filename, base64url-decoded and sanitised; `file` when absent
/// or unusable. The name is cosmetic — the id is what makes the path unique.
fn filename(headers: &HeaderMap) -> String {
    let decoded = headers
        .get(FILENAME_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| base64url_decode(value.trim()))
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    let sanitised: String = decoded
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_FILENAME)
        .collect();
    let trimmed = sanitised.trim_matches(['.', '_']).to_owned();
    if trimmed.is_empty() {
        "file".to_owned()
    } else {
        trimmed
    }
}

/// base64url (with or without padding). Written out rather than pulled in as a
/// dependency: it is fifteen lines and the crate has no other use for base64.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}

fn io_error(error: &std::io::Error) -> KohralError {
    tracing::error!(error = %error, "writing a Kohral attachment failed");
    KohralError::new(
        KohralErrorCode::StorageUnavailable,
        "the attachment could not be stored",
    )
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;

    use super::{FILENAME_HEADER, base64url_decode, directory, filename, safe_segment};

    #[test]
    fn path_segments_cannot_escape_the_upload_root() {
        assert!(safe_segment("conv-1").is_ok());
        assert!(safe_segment("..").is_err());
        assert!(safe_segment(".").is_err());
        assert!(safe_segment("a/b").is_err());
        assert!(safe_segment("a\\b").is_err());
        assert!(safe_segment("").is_err());
        assert!(safe_segment(&"x".repeat(129)).is_err());

        let root = std::path::Path::new("/tmp/kohral-uploads");
        assert!(directory(root, "../../etc", "m").is_err());
        assert_eq!(
            directory(root, "c", "m").expect("safe"),
            std::path::Path::new("/tmp/kohral-uploads/c/m")
        );
    }

    #[test]
    fn the_filename_header_is_decoded_and_sanitised() {
        let mut headers = HeaderMap::new();
        // base64url of "../../etc/passwd"
        headers.insert(
            FILENAME_HEADER,
            "Li4vLi4vZXRjL3Bhc3N3ZA".parse().expect("header"),
        );
        let name = filename(&headers);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.contains(".."), "{name}");
        assert_eq!(name, "etc_passwd");

        assert_eq!(filename(&HeaderMap::new()), "file");
    }

    #[test]
    fn base64url_accepts_both_alphabets_and_optional_padding() {
        assert_eq!(base64url_decode("aGVsbG8=").expect("decoded"), b"hello");
        assert_eq!(base64url_decode("aGVsbG8").expect("decoded"), b"hello");
        assert!(base64url_decode("not base64!").is_none());
    }
}
