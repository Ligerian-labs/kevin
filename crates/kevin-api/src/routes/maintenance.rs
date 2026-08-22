//! `/api/v1/maintenance/drain` — the orchestrator's admission gate
//! (`plan/10-observability-ops.md` §Health and drain).
//!
//! Draining stops the runtime from accepting new runs while the ones in flight
//! finish; `/readyz` turns 503 so a load balancer stops routing work here, and
//! `/healthz` stays 200 so nothing restarts the process.
//!
//! Loopback only: this is an operator verb, not a client one.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::dto::DrainStatusDto;
use crate::error::ApiError;
use crate::routes::Peer;
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route(
        "/maintenance/drain",
        get(drain_status).post(start_drain).delete(stop_drain),
    )
}

/// `GET /api/v1/maintenance/drain`.
#[utoipa::path(
    get, path = "/api/v1/maintenance/drain", tag = "maintenance",
    responses((status = 200, body = DrainStatusDto), (status = 403, description = "forbidden")),
    security(("bearer" = []))
)]
pub async fn drain_status(
    State(state): State<AppState>,
    peer: Peer,
) -> Result<Json<DrainStatusDto>, ApiError> {
    peer.require_loopback()?;
    Ok(Json(state.runtime().drain_status().await?))
}

/// `POST /api/v1/maintenance/drain` — close admission.
#[utoipa::path(
    post, path = "/api/v1/maintenance/drain", tag = "maintenance",
    responses((status = 200, body = DrainStatusDto), (status = 403, description = "forbidden")),
    security(("bearer" = []))
)]
pub async fn start_drain(
    State(state): State<AppState>,
    peer: Peer,
) -> Result<Json<DrainStatusDto>, ApiError> {
    peer.require_loopback()?;
    Ok(Json(state.runtime().set_drain(true).await?))
}

/// `DELETE /api/v1/maintenance/drain` — reopen admission.
#[utoipa::path(
    delete, path = "/api/v1/maintenance/drain", tag = "maintenance",
    responses((status = 200, body = DrainStatusDto), (status = 403, description = "forbidden")),
    security(("bearer" = []))
)]
pub async fn stop_drain(
    State(state): State<AppState>,
    peer: Peer,
) -> Result<Json<DrainStatusDto>, ApiError> {
    peer.require_loopback()?;
    Ok(Json(state.runtime().set_drain(false).await?))
}
