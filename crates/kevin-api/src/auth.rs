//! Bearer authentication (`plan/07-api-and-tui.md` §Authentication,
//! `plan/09-security.md` T5).
//!
//! - `Authorization: Bearer <token>`; the token comes from
//!   `server.auth_token_file` (0600, 32 random bytes base64url written by
//!   `kevin config init`).
//! - The comparison is **constant time**: both sides are SHA-256'd (so the
//!   length of the presented token leaks nothing either) and the digests are
//!   compared with [`subtle::ConstantTimeEq`].
//! - `kevin config rotate-token` + `SIGHUP` calls [`TokenVerifier::reload`],
//!   which keeps accepting the previous token for `server.token_grace`.
//! - Exempt from auth: `/healthz`, `/readyz`, `/metrics` (a separate listener)
//!   and `/api/v1/openapi.json`.
//! - The token is never logged, never serialised and never put in an error
//!   message.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::error::{ApiError, ErrorCode};
use crate::state::AppState;

/// SHA-256 digest of a token; the only form a token is kept in memory in.
type Digest = [u8; 32];

fn digest(token: &str) -> Digest {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

#[derive(Debug)]
struct Tokens {
    current: Digest,
    /// Digest of the token replaced by the last [`TokenVerifier::reload`] and
    /// the instant it stopped being current.
    previous: Option<(Digest, Instant)>,
}

/// Verifies bearer tokens in constant time, with a rotation grace window.
#[derive(Debug)]
pub struct TokenVerifier {
    tokens: RwLock<Tokens>,
    grace: Duration,
    path: Option<PathBuf>,
}

impl TokenVerifier {
    /// A verifier for a literal token (tests, embedded runtime).
    #[must_use]
    pub fn new(token: &str) -> Self {
        Self {
            tokens: RwLock::new(Tokens {
                current: digest(token),
                previous: None,
            }),
            grace: Duration::from_secs(300),
            path: None,
        }
    }

    /// Reads `server.auth_token_file`. The file must exist and be non-empty —
    /// an API without a token is never started (plan/09 T5).
    pub fn from_file(path: impl AsRef<Path>, grace: Duration) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let token = read_token(&path)?;
        Ok(Self {
            tokens: RwLock::new(Tokens {
                current: digest(&token),
                previous: None,
            }),
            grace,
            path: Some(path),
        })
    }

    /// Sets the rotation grace window (`server.token_grace`).
    #[must_use]
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Re-reads the token file (SIGHUP). The old token keeps working for
    /// `server.token_grace`, so a rotation needs no downtime.
    pub fn reload(&self) -> std::io::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let token = read_token(path)?;
        let new = digest(&token);
        let mut tokens = self
            .tokens
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tokens.current != new {
            tokens.previous = Some((tokens.current, Instant::now()));
            tokens.current = new;
        }
        Ok(())
    }

    /// Whether `presented` is the current token, or the previous one inside the
    /// grace window. Runs in constant time with respect to the token bytes.
    #[must_use]
    pub fn verify(&self, presented: &str) -> bool {
        let presented = digest(presented);
        let tokens = self
            .tokens
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut ok = tokens.current.ct_eq(&presented);
        if let Some((previous, rotated_at)) = tokens.previous
            && rotated_at.elapsed() <= self.grace
        {
            ok |= previous.ct_eq(&presented);
        }
        ok.into()
    }
}

fn read_token(path: &Path) -> std::io::Result<String> {
    let token = std::fs::read_to_string(path)?.trim().to_owned();
    if token.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the auth token file is empty (run `kevin config init`)",
        ));
    }
    Ok(token)
}

/// Paths that never require a token (plan/07 §Authentication).
#[must_use]
pub fn is_exempt(path: &str) -> bool {
    matches!(
        path,
        "/healthz" | "/readyz" | "/metrics" | "/api/v1/openapi.json" | "/api/v1/docs"
    )
}

/// The bearer token of `request`, if it presents a well-formed one.
fn bearer(request: &Request) -> Option<&str> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Middleware enforcing the bearer token on every non-exempt route.
pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if is_exempt(request.uri().path()) {
        return next.run(request).await;
    }
    let Some(token) = bearer(&request) else {
        tracing::debug!(event = "kevin.api.auth_failed", reason = "missing");
        return unauthenticated().into_response();
    };
    if !state.auth().verify(token) {
        tracing::warn!(event = "kevin.api.auth_failed", reason = "invalid");
        return unauthenticated().into_response();
    }
    next.run(request).await
}

fn unauthenticated() -> ApiError {
    ApiError::new(
        ErrorCode::Unauthenticated,
        "a valid `Authorization: Bearer <token>` header is required",
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::TokenVerifier;

    #[test]
    fn only_the_configured_token_verifies() {
        let verifier = TokenVerifier::new("s3cret");
        assert!(verifier.verify("s3cret"));
        assert!(!verifier.verify("s3crey"));
        assert!(!verifier.verify(""));
        // A different length must not short-circuit: both sides are hashed.
        assert!(!verifier.verify("s3cret-and-then-some-very-long-suffix"));
    }

    #[test]
    fn rotation_keeps_the_old_token_for_the_grace_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "old-token\n").expect("write");

        let verifier =
            TokenVerifier::from_file(&path, Duration::from_secs(300)).expect("read token");
        assert!(verifier.verify("old-token"));

        std::fs::write(&path, "new-token\n").expect("rotate");
        verifier.reload().expect("reload");
        assert!(verifier.verify("new-token"), "the new token is accepted");
        assert!(
            verifier.verify("old-token"),
            "the old one is still in grace"
        );

        let expired = TokenVerifier::new("x").with_grace(Duration::ZERO);
        assert!(!expired.verify("y"));
    }

    #[test]
    fn an_empty_token_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("token");
        std::fs::write(&path, "   \n").expect("write");
        assert!(TokenVerifier::from_file(&path, Duration::from_secs(1)).is_err());
    }
}
