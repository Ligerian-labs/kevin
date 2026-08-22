//! `/api/v1/workers` — `Worker::doctor()` for every configured worker.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::dto::WorkerDoctorDto;
use crate::error::ApiError;
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route("/workers", get(doctor))
}

/// `GET /api/v1/workers`.
#[utoipa::path(
    get, path = "/api/v1/workers", tag = "workers",
    responses((status = 200, body = Vec<WorkerDoctorDto>),
              (status = 503, description = "runtime_unavailable")),
    security(("bearer" = []))
)]
pub async fn doctor(State(state): State<AppState>) -> Result<Json<Vec<WorkerDoctorDto>>, ApiError> {
    Ok(Json(state.workers()?.doctor().await?))
}
