//! Kevin's HTTP API (`plan/07-api-and-tui.md` §1–2).
//!
//! # What is here
//!
//! - [`router`] — the axum router: everything under `/api/v1`, plus the
//!   unversioned `/healthz` and `/readyz`. **`/metrics` is not served here**:
//!   per `plan/10-observability-ops.md` §Metrics it belongs to the separate
//!   `telemetry.metrics_bind` listener, so scraping never competes with API
//!   traffic and never needs the API token.
//! - [`dto`] — the wire types, `serde` + `utoipa::ToSchema`.
//! - [`error`] — the stable `{code, message, details?, request_id}` envelope
//!   and the full code table.
//! - [`auth`] — bearer token, constant-time compare, SIGHUP-able rotation.
//! - [`sse`] — catch-up + live merge with `Last-Event-ID`, `resync` on bus lag.
//! - [`port`] — the narrow traits the HTTP layer is written against, so the
//!   whole surface is testable without Postgres or a worker subprocess.
//! - [`runtime`] — the production [`port::RuntimePort`] over WS-08's
//!   `Orchestrator` services; [`adapters`] holds the read-side ones.
//! - [`sse_wire`] — the `text/event-stream` decoder both sides share.
//! - [`client`] — `KevinClient`, behind the `client` feature (no axum).
//!
//! # Shape of a request
//!
//! ```text
//! x-request-id → task-local ─┐
//! body limit (1 MiB)         │
//! CORS (server.cors_origins)  │
//! bearer token (constant time)│  every /api/v1 route except openapi.json
//! rate limit (60/s, burst 120)│
//! request timeout             │  everything except SSE
//! handler ────────────────────┘ → DTO   | ApiError → {code,…,request_id}
//! ```
//!
//! Features: `server` (default) is the axum router; `client` is the typed
//! HTTP client used by `kevin-tui` and `kevin-cli` and pulls neither axum nor
//! the orchestrator.
//!
//! # Wiring
//!
//! ```text
//! Orchestrator::boot ──► Handle ──► runtime::OrchestratorRuntime  (writes)
//! ReadModels         ─────────────► adapters::ProjectionReads     (reads)
//! EventStore + Bus   ─────────────► adapters::StoreEvents         (SSE)
//! WorkerRegistry / MemoryStore / RouteScoreRepo ──► adapters::*
//!                                  └──► AppState ──► router(state)
//! ```

pub mod dto;
pub mod error;
pub mod sse_wire;

#[cfg(feature = "server")]
pub mod adapters;
#[cfg(feature = "server")]
pub mod auth;
#[cfg(feature = "server")]
pub mod convert;
#[cfg(feature = "server")]
pub mod openapi;
#[cfg(feature = "server")]
pub mod port;
#[cfg(feature = "server")]
pub mod request_id;
#[cfg(feature = "server")]
pub mod routes;
#[cfg(feature = "server")]
pub mod runtime;
#[cfg(feature = "server")]
pub mod sse;
#[cfg(feature = "server")]
pub mod state;

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "server")]
pub use server::{ApiOptions, router, router_with};

#[cfg(feature = "server")]
mod server {
    use std::time::Duration;
    use std::time::Instant;

    use axum::extract::{DefaultBodyLimit, MatchedPath, State};
    use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
    use axum::response::{Html, IntoResponse, Response};
    use axum::routing::get;
    use axum::{Json, Router, middleware};
    use tower_http::cors::{Any, CorsLayer};
    use tower_http::timeout::TimeoutLayer;
    use tower_http::trace::TraceLayer;

    use crate::auth;
    use crate::error::{ApiError, ErrorCode};
    use crate::openapi::{ApiDoc, DOCS_HTML};
    use crate::request_id;
    use crate::routes;
    use crate::state::{AppState, MAX_BODY_BYTES};

    /// Knobs the caller may override on top of `[server]`.
    #[derive(Debug, Clone)]
    pub struct ApiOptions {
        /// Per-request timeout; SSE routes are exempt.
        pub request_timeout: Duration,
        /// Allowed CORS origins; empty disables CORS entirely.
        pub cors_origins: Vec<String>,
        /// Serve the Swagger UI at `/api/v1/docs`.
        pub docs: bool,
    }

    impl ApiOptions {
        /// The options implied by a `[server]` section.
        #[must_use]
        pub fn from_config(server: &kevin_config::schema::Server) -> Self {
            Self {
                request_timeout: server.request_timeout,
                cors_origins: server.cors_origins.clone(),
                docs: server.docs,
            }
        }
    }

    /// The complete API router.
    ///
    /// SSE routes are mounted **outside** the timeout layer (a stream is
    /// supposed to stay open) but inside auth and the rate limiter.
    pub fn router(state: AppState) -> Router {
        let options = ApiOptions::from_config(state.server());
        router_with(state, &options)
    }

    /// [`router`] with explicit options (embedded runtime, tests).
    pub fn router_with(state: AppState, options: &ApiOptions) -> Router {
        let streaming = Router::new()
            .merge(routes::events::router())
            .with_state(state.clone());

        let versioned = Router::new()
            .merge(routes::v1(state.clone()))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                options.request_timeout,
            ))
            .merge(streaming)
            .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth::require_token,
            ));

        let unauthenticated = Router::new()
            .route("/openapi.json", get(openapi_json))
            .merge(docs_route(options.docs));

        let mut app = Router::new()
            .nest("/api/v1", versioned.merge(unauthenticated))
            .merge(routes::health::router().with_state(state))
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .layer(middleware::from_fn(record_request))
            .layer(middleware::from_fn(request_id::layer))
            .layer(TraceLayer::new_for_http());

        if let Some(cors) = cors_layer(&options.cors_origins) {
            app = app.layer(cors);
        }
        app.fallback(not_found)
    }

    /// `kevin_api_requests_total` / `kevin_api_request_duration_seconds`
    /// (plan/10 §Metrics).
    ///
    /// The `route` label is the **matched path template**, never the concrete
    /// URI, so an endpoint taking an id contributes one series and not one per
    /// run. Unmatched requests are folded into `"other"`.
    async fn record_request(request: axum::extract::Request, next: middleware::Next) -> Response {
        let method = method_label(request.method());
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map_or_else(|| "other".to_owned(), |p| p.as_str().to_owned());
        let started = Instant::now();
        let response = next.run(request).await;
        let status_class = match response.status().as_u16() {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        };
        metrics::counter!(
            kevin_telemetry::metrics::API_REQUESTS_TOTAL,
            "route" => route.clone(),
            "method" => method,
            "status_class" => status_class,
        )
        .increment(1);
        metrics::histogram!(
            kevin_telemetry::metrics::API_REQUEST_DURATION_SECONDS,
            "route" => route,
            "method" => method,
        )
        .record(started.elapsed().as_secs_f64());
        response
    }

    /// Bounded `method` label: anything exotic becomes `"other"`.
    fn method_label(method: &Method) -> &'static str {
        match *method {
            Method::GET => "GET",
            Method::POST => "POST",
            Method::PUT => "PUT",
            Method::DELETE => "DELETE",
            Method::PATCH => "PATCH",
            Method::HEAD => "HEAD",
            Method::OPTIONS => "OPTIONS",
            _ => "other",
        }
    }

    /// `GET /api/v1/openapi.json` — exempt from auth (plan/07).
    async fn openapi_json() -> Json<serde_json::Value> {
        Json(ApiDoc::json())
    }

    fn docs_route(enabled: bool) -> Router {
        if enabled {
            Router::new().route("/docs", get(|| async { Html(DOCS_HTML) }))
        } else {
            Router::new()
        }
    }

    async fn not_found() -> Response {
        ApiError::new(ErrorCode::InvalidRequest, "no such endpoint").into_response()
    }

    /// Per-token token bucket (plan/07 §Limits).
    async fn rate_limit(
        State(state): State<AppState>,
        request: axum::extract::Request,
        next: middleware::Next,
    ) -> Response {
        let key = routes::rate_key(request.headers());
        if state.rate_limiter().allow(&key) {
            return next.run(request).await;
        }
        ApiError::new(ErrorCode::RateLimited, "too many requests").into_response()
    }

    fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
        if origins.is_empty() {
            return None;
        }
        let methods = [Method::GET, Method::POST, Method::DELETE, Method::OPTIONS];
        let headers = [
            HeaderName::from_static("authorization"),
            HeaderName::from_static("content-type"),
            HeaderName::from_static("idempotency-key"),
            HeaderName::from_static("last-event-id"),
            HeaderName::from_static("x-request-id"),
        ];
        if origins.iter().any(|origin| origin == "*") {
            return Some(
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(methods)
                    .allow_headers(headers),
            );
        }
        let parsed: Vec<HeaderValue> = origins
            .iter()
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect();
        Some(
            CorsLayer::new()
                .allow_origin(parsed)
                .allow_methods(methods)
                .allow_headers(headers)
                .allow_credentials(true),
        )
    }
}
