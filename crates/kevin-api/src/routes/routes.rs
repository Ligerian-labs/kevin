//! `/api/v1/routes` — the routing leaderboard (`routing.route_leaderboard`).

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::routing::get;

use crate::dto::{RouteScoreDto, RoutesQuery};
use crate::error::ApiError;
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route("/routes", get(leaderboard))
}

/// `GET /api/v1/routes`.
#[utoipa::path(
    get, path = "/api/v1/routes", tag = "routes",
    params(("kind" = Option<String>, Query, description = "Keep only this task kind")),
    responses((status = 200, body = Vec<RouteScoreDto>),
              (status = 503, description = "runtime_unavailable")),
    security(("bearer" = []))
)]
pub async fn leaderboard(
    State(state): State<AppState>,
    query: Result<Query<RoutesQuery>, QueryRejection>,
) -> Result<Json<Vec<RouteScoreDto>>, ApiError> {
    let Query(query) = query?;
    Ok(Json(
        state.router()?.leaderboard(query.kind.as_deref()).await?,
    ))
}
