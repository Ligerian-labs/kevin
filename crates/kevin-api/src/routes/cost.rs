//! `/api/v1/cost` — the grouped spend report (`orch.cost_ledger`).

use axum::Json;
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::routing::get;

use crate::dto::{CostQueryDto, CostReportDto};
use crate::error::ApiError;
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route("/cost", get(cost))
}

/// `GET /api/v1/cost`.
#[utoipa::path(
    get, path = "/api/v1/cost", tag = "cost",
    params(("since" = Option<String>, Query, description = "RFC 3339 lower bound"),
           ("run_id" = Option<String>, Query),
           ("group_by" = Option<String>, Query, description = "run | model | kind")),
    responses((status = 200, body = CostReportDto)),
    security(("bearer" = []))
)]
pub async fn cost(
    State(state): State<AppState>,
    query: Result<Query<CostQueryDto>, QueryRejection>,
) -> Result<Json<CostReportDto>, ApiError> {
    let Query(query) = query?;
    if let Some(group_by) = query.group_by.as_deref()
        && !matches!(group_by, "run" | "model" | "kind")
    {
        return Err(ApiError::invalid_request(
            "`group_by` must be one of run, model, kind",
        ));
    }
    Ok(Json(state.read().cost(&query).await?))
}
