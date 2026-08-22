//! Request ids (`plan/07-api-and-tui.md` §1).
//!
//! `x-request-id` is honoured when the client sends one and generated (uuid
//! v7) otherwise. It is echoed on the response, put in the error envelope, and
//! used as the `causation_id` of every command the request issues.
//!
//! The id lives in a **tokio task-local** rather than a request extension so
//! that [`crate::error::ApiError::into_response`] — which runs far away from
//! the extractor — can stamp it without every handler threading it through.

use std::future::Future;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// The header carrying the request id, in and out.
pub const HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Longest client-supplied id we echo back (longer ones are replaced).
const MAX_LEN: usize = 128;

tokio::task_local! {
    static REQUEST_ID: String;
}

/// Access to the current request id.
#[derive(Debug, Clone, Copy)]
pub struct RequestId;

impl RequestId {
    /// The id of the request being handled on this task, when there is one.
    #[must_use]
    pub fn current() -> Option<String> {
        REQUEST_ID.try_with(Clone::clone).ok()
    }

    /// Runs `f` with `id` as the current request id.
    pub async fn scope<F: Future>(id: String, f: F) -> F::Output {
        REQUEST_ID.scope(id, f).await
    }
}

/// Middleware: resolve the id, expose it as a task-local for the whole
/// handler, and echo it on the response.
pub async fn layer(mut request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= MAX_LEN && v.is_ascii())
        .map_or_else(|| uuid::Uuid::now_v7().to_string(), ToOwned::to_owned);

    if let Ok(value) = HeaderValue::from_str(&id) {
        request.headers_mut().insert(HEADER, value);
    }

    let echoed = id.clone();
    let mut response: Response<Body> = RequestId::scope(id, next.run(request)).await;
    if let Ok(value) = HeaderValue::from_str(&echoed) {
        response.headers_mut().insert(HEADER, value);
    }
    response
}
