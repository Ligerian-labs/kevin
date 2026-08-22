//! The error envelope of the Kohral surface (`plan/08-kohral-runtime.md` §1.1).
//!
//! Kohral has two parsers for a runtime error: `HermesRuntimeStrategy` reads
//! the OpenAI-shaped `{"error": {"message", "type", "code"}}` body, while the
//! conformance script and the drain/catalog helpers read a bare top-level
//! `code`. Every error therefore carries **both**, and the `code` is always
//! one of the stable [`KohralErrorCode`] strings so Kohral can classify a
//! failure without reading prose.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Stable machine codes of the Kohral surface.
///
/// They double as `error_code` values on a failed run, so they match
/// Kohral's `^[a-z][a-z0-9_]{1,63}$`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KohralErrorCode {
    /// The bearer token is missing or wrong (`401`).
    InvalidApiKey,
    /// `Idempotency-Key` is missing or malformed (`400`).
    InvalidIdempotencyKey,
    /// The request body is not a turn (`400`).
    InvalidRequest,
    /// `model` names something the catalog does not offer (`400`).
    UnknownModel,
    /// The key was reused with a different request (`409`).
    IdempotencyConflict,
    /// No run with that id (`404`).
    RunNotFound,
    /// No session with that id (`404`).
    SessionNotFound,
    /// The runtime is draining and refuses new turns (`503`).
    GatewayDraining,
    /// The model catalog could not be built (`503`).
    ModelCatalogUnavailable,
    /// An attachment failed validation (`400`).
    InvalidAttachment,
    /// The attachment exceeds `kohral.max_attachment_bytes` (`413`).
    AttachmentTooLarge,
    /// The ledger or the store did not answer (`503`).
    StorageUnavailable,
    /// Anything else (`500`).
    InternalError,
}

impl KohralErrorCode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            KohralErrorCode::InvalidApiKey => "invalid_api_key",
            KohralErrorCode::InvalidIdempotencyKey => "invalid_idempotency_key",
            KohralErrorCode::InvalidRequest => "invalid_request",
            KohralErrorCode::UnknownModel => "unknown_model",
            KohralErrorCode::IdempotencyConflict => "idempotency_conflict",
            KohralErrorCode::RunNotFound => "run_not_found",
            KohralErrorCode::SessionNotFound => "session_not_found",
            KohralErrorCode::GatewayDraining => "gateway_draining",
            KohralErrorCode::ModelCatalogUnavailable => "model_catalog_unavailable",
            KohralErrorCode::InvalidAttachment => "invalid_attachment",
            KohralErrorCode::AttachmentTooLarge => "attachment_too_large",
            KohralErrorCode::StorageUnavailable => "storage_unavailable",
            KohralErrorCode::InternalError => "internal_error",
        }
    }

    /// The OpenAI-style `error.type` Hermes reports for this class of failure.
    #[must_use]
    pub const fn error_type(self) -> &'static str {
        match self {
            KohralErrorCode::StorageUnavailable
            | KohralErrorCode::ModelCatalogUnavailable
            | KohralErrorCode::GatewayDraining
            | KohralErrorCode::InternalError => "server_error",
            _ => "invalid_request_error",
        }
    }

    /// The HTTP status this code is reported with.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            KohralErrorCode::InvalidApiKey => StatusCode::UNAUTHORIZED,
            KohralErrorCode::InvalidIdempotencyKey
            | KohralErrorCode::InvalidRequest
            | KohralErrorCode::UnknownModel
            | KohralErrorCode::InvalidAttachment => StatusCode::BAD_REQUEST,
            KohralErrorCode::IdempotencyConflict => StatusCode::CONFLICT,
            KohralErrorCode::RunNotFound | KohralErrorCode::SessionNotFound => {
                StatusCode::NOT_FOUND
            }
            KohralErrorCode::AttachmentTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            KohralErrorCode::GatewayDraining
            | KohralErrorCode::ModelCatalogUnavailable
            | KohralErrorCode::StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            KohralErrorCode::InternalError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// One error of the Kohral surface, rendered in both shapes Kohral parses.
#[derive(Debug, Clone)]
pub struct KohralError {
    code: KohralErrorCode,
    message: String,
}

impl KohralError {
    /// An error with an explicit message.
    pub fn new(code: KohralErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The `401` the conformance script asserts on a wrong token.
    #[must_use]
    pub fn invalid_api_key() -> Self {
        Self::new(KohralErrorCode::InvalidApiKey, "Invalid API key")
    }

    /// The machine code.
    #[must_use]
    pub const fn code(&self) -> KohralErrorCode {
        self.code
    }

    /// The body Kohral receives.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        json!({
            "code": self.code.as_str(),
            "error": {
                "message": self.message,
                "type": self.code.error_type(),
                "code": self.code.as_str(),
            }
        })
    }
}

impl std::fmt::Display for KohralError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for KohralError {}

impl IntoResponse for KohralError {
    fn into_response(self) -> Response {
        (self.code.status(), Json(self.body())).into_response()
    }
}

impl From<sqlx::Error> for KohralError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "kohral ledger query failed");
        Self::new(
            KohralErrorCode::StorageUnavailable,
            "the runtime ledger is unavailable",
        )
    }
}

/// Shorthand result of a Kohral handler.
pub type KohralResult<T> = Result<T, KohralError>;

#[cfg(test)]
mod tests {
    use super::{KohralError, KohralErrorCode};

    #[test]
    fn both_parsers_find_the_code() {
        let body = KohralError::invalid_api_key().body();
        assert_eq!(body["code"], "invalid_api_key");
        assert_eq!(body["error"]["code"], "invalid_api_key");
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], "Invalid API key");
    }

    #[test]
    fn every_code_matches_kohrals_error_code_pattern() {
        let pattern = regex::Regex::new("^[a-z][a-z0-9_]{1,63}$").expect("valid pattern");
        for code in [
            KohralErrorCode::InvalidApiKey,
            KohralErrorCode::InvalidIdempotencyKey,
            KohralErrorCode::InvalidRequest,
            KohralErrorCode::UnknownModel,
            KohralErrorCode::IdempotencyConflict,
            KohralErrorCode::RunNotFound,
            KohralErrorCode::SessionNotFound,
            KohralErrorCode::GatewayDraining,
            KohralErrorCode::ModelCatalogUnavailable,
            KohralErrorCode::InvalidAttachment,
            KohralErrorCode::AttachmentTooLarge,
            KohralErrorCode::StorageUnavailable,
            KohralErrorCode::InternalError,
        ] {
            assert!(pattern.is_match(code.as_str()), "{}", code.as_str());
        }
    }
}
