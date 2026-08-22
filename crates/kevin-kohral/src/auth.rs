//! Bearer authentication of the Kohral listener
//! (`plan/08-kohral-runtime.md` §1.1, `plan/07-api-and-tui.md` §Authentication).
//!
//! The Kohral surface has **its own** token, mounted by Kohral at
//! `kohral.token_file` (secret binding `KEVIN_RUNTIME_TOKEN → API_SERVER_KEY`);
//! it is never the `[server]` API token, so a leaked operator token does not
//! let anyone submit turns and vice versa. Verification reuses
//! [`kevin_api::auth::TokenVerifier`] (SHA-256 + [`subtle`] constant-time
//! compare, rotation grace), only the failure body differs: Kohral wants the
//! Hermes error envelope, not Kevin's.
//!
//! `/health` and `/v1/health` are the only unauthenticated routes — Kohral's
//! `RuntimeTelemetry::health()` polls them without secrets.

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::KohralError;
use crate::state::KohralState;

/// Routes Kohral polls without a token.
#[must_use]
pub fn is_exempt(path: &str) -> bool {
    matches!(path, "/health" | "/v1/health")
}

/// The presented bearer token, if the header is well formed.
fn bearer(request: &Request) -> Option<&str> {
    let value = request.headers().get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

/// Middleware enforcing `Authorization: Bearer <kohral token>`.
pub async fn require_token(
    State(state): State<KohralState>,
    request: Request,
    next: Next,
) -> Response {
    if is_exempt(request.uri().path()) {
        return next.run(request).await;
    }
    let presented = bearer(&request);
    let ok = presented.is_some_and(|token| state.auth().verify(token));
    if !ok {
        tracing::warn!(
            event = "kevin.api.auth_failed",
            surface = "kohral",
            reason = if presented.is_some() {
                "invalid"
            } else {
                "missing"
            },
        );
        return KohralError::invalid_api_key().into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::is_exempt;

    #[test]
    fn only_health_is_unauthenticated() {
        assert!(is_exempt("/health"));
        assert!(is_exempt("/v1/health"));
        assert!(!is_exempt("/health/detailed"));
        assert!(!is_exempt("/v1/capabilities"));
        assert!(!is_exempt("/v1/kohral/models"));
        assert!(!is_exempt("/v1/runs"));
    }
}
