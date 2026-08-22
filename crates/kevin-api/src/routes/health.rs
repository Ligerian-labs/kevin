//! Unversioned health endpoints (`plan/10-observability-ops.md` §Health and
//! drain). They are exempt from authentication — they are protected by the
//! bind address and carry no information beyond "alive" / "ready".
//!
//! - `/healthz` is **liveness**: it never touches the database, so a database
//!   outage does not make an orchestrator kill a healthy process.
//! - `/readyz` is **readiness**: db reachable, startup finished, workers
//!   healthy, not draining. It returns `503` while draining so a load balancer
//!   stops sending new runs.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

use crate::dto::{HealthDto, ReadyDto};
use crate::state::AppState;

/// The unversioned health routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

/// `GET /healthz`.
#[utoipa::path(
    get, path = "/healthz", tag = "health",
    responses((status = 200, body = HealthDto))
)]
pub async fn healthz() -> Json<HealthDto> {
    Json(HealthDto {
        status: "ok".to_owned(),
    })
}

/// `GET /readyz`.
#[utoipa::path(
    get, path = "/readyz", tag = "health",
    responses((status = 200, body = ReadyDto, description = "Ready for new runs"),
              (status = 503, body = ReadyDto, description = "Draining or a dependency is down"))
)]
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<ReadyDto>) {
    let readiness = state.runtime().readiness().await;
    let dto = ReadyDto {
        ready: readiness.ready(),
        db: readiness.db,
        draining: readiness.draining,
        workers_ok: readiness.workers_ok,
    };
    let status = if dto.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(dto))
}
