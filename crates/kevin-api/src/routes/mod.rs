//! The HTTP surface: one module per resource, exactly the endpoint table of
//! `plan/07-api-and-tui.md` §Endpoints.
//!
//! `/metrics` is **not** served here: per plan/10 §Metrics it lives on
//! `telemetry.metrics_bind`, a separate listener, so scraping never competes
//! with API traffic and never needs the API token.

pub mod config;
pub mod cost;
pub mod events;
pub mod health;
pub mod maintenance;
pub mod memory;
pub mod proposals;
pub mod questions;
// The plan (`plan/07` §Module layout) names this module `routes`; the
// repetition is part of the frozen layout.
#[allow(clippy::module_inception)]
pub mod routes;
pub mod runs;
pub mod tasks;
pub mod workers;

use std::convert::Infallible;
use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use kevin_domain::Actor;
use serde::Serialize;

use crate::error::{ApiError, ErrorCode};
use crate::port::CommandCtx;
use crate::request_id::RequestId;
use crate::state::{AppState, Idempotency, MAX_PAGE_LIMIT, Replay, body_hash};

/// The `Actor` every API-issued command is attributed to.
#[must_use]
pub fn api_actor() -> Actor {
    Actor::user("api")
}

/// The non-streaming `/api/v1` routes.
///
/// The SSE routes live in [`events::router`] and are mounted separately by
/// [`crate::router`], **outside** the request timeout (a stream is meant to
/// stay open) but inside auth and the rate limiter.
pub fn v1(state: AppState) -> Router {
    Router::new()
        .merge(runs::router())
        .merge(tasks::router())
        .merge(questions::router())
        .merge(cost::router())
        .merge(routes::router())
        .merge(memory::router())
        .merge(proposals::router())
        .merge(workers::router())
        .merge(config::router())
        .merge(maintenance::router())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Caps a client-supplied `?limit=` at [`MAX_PAGE_LIMIT`].
#[must_use]
pub fn clamp_limit(limit: Option<usize>) -> Option<usize> {
    limit.map(|value| value.clamp(1, MAX_PAGE_LIMIT))
}

/// The command context for a request: the `Idempotency-Key` (or a fresh id) as
/// `command_id`, the `x-request-id` as `causation_id`.
#[must_use]
pub fn command_ctx(key: Option<&str>) -> CommandCtx {
    CommandCtx {
        command_id: key.map_or_else(kevin_domain::ids::CommandId::new, Idempotency::command_id),
        causation_id: RequestId::current().and_then(|id| id.parse().ok()),
        actor: api_actor(),
    }
}

/// Reads and validates the `Idempotency-Key` header.
pub fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| ApiError::invalid_request("Idempotency-Key must be ASCII"))?;
    Idempotency::validate(key)?;
    Ok(Some(key.to_owned()))
}

/// Runs `command` under an `Idempotency-Key`.
///
/// A replay with the same body returns the original response with `200`; a
/// replay with a different body is `409 idempotency_conflict`.
pub async fn idempotent<T, F, Fut>(
    state: &AppState,
    key: Option<&str>,
    body: &[u8],
    created: axum::http::StatusCode,
    command: F,
) -> Result<Response, ApiError>
where
    T: Serialize,
    F: FnOnce(CommandCtx) -> Fut,
    Fut: Future<Output = Result<T, ApiError>>,
{
    let ctx = command_ctx(key);
    let Some(key) = key else {
        let value = command(ctx).await?;
        return Ok((created, Json(value)).into_response());
    };

    let hash = body_hash(body);
    match state.idempotency().check(key, hash) {
        Replay::Same(response) => {
            return Ok((axum::http::StatusCode::OK, Json(response)).into_response());
        }
        Replay::Conflict => {
            return Err(ApiError::new(
                ErrorCode::IdempotencyConflict,
                "this Idempotency-Key was already used with a different request body",
            )
            .with_details(serde_json::json!({ "idempotency_key": key })));
        }
        Replay::Fresh => {}
    }

    let value = command(ctx).await?;
    let json = serde_json::to_value(&value).map_err(|e| ApiError::internal(e.to_string()))?;
    state.idempotency().remember(key, hash, json.clone());
    Ok((created, Json(json)).into_response())
}

/// Parses a JSON body that may legitimately be empty (`{}` bodies of the
/// command endpoints).
pub fn parse_body<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(body)
        .map_err(|e| ApiError::invalid_request(format!("invalid JSON body: {e}")))
}

/// The peer address of a request, when the server was started with
/// `into_make_service_with_connect_info`. In-process calls (`oneshot`) have
/// none and count as loopback.
#[derive(Debug, Clone, Copy)]
pub struct Peer(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Peer {
    type Rejection = Infallible;

    #[allow(
        clippy::unused_async_trait_impl,
        reason = "the extractor trait declares an async fn"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Peer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        ))
    }
}

impl Peer {
    /// Refuses a request that did not come from the loopback interface
    /// (`403 forbidden`, plan/07 §Conventions).
    pub fn require_loopback(self) -> Result<(), ApiError> {
        if crate::state::is_loopback(self.0) {
            return Ok(());
        }
        Err(ApiError::new(
            ErrorCode::Forbidden,
            "this endpoint is only reachable from the loopback interface",
        ))
    }
}

/// The rate-limit bucket key of a request: a **hash** of the bearer token, so
/// the limiter's map never holds a credential in the clear.
#[must_use]
pub fn rate_key(headers: &HeaderMap) -> String {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .map(axum::http::HeaderValue::as_bytes)
    else {
        return "anonymous".to_owned();
    };
    body_hash(value)
        .iter()
        .fold(String::new(), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}
