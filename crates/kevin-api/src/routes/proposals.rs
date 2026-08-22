//! `/api/v1/proposals` — the evaluator's inbox (`eval.proposals_inbox`).

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use kevin_domain::ids::ProposalId;
use uuid::Uuid;

use crate::dto::{Page, ProposalDecisionRequest, ProposalDto, ProposalsQuery};
use crate::error::ApiError;
use crate::routes::{clamp_limit, command_ctx, idempotency_key};
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/proposals", get(list))
        .route("/proposals/{proposal_id}/accept", post(accept))
        .route("/proposals/{proposal_id}/reject", post(reject))
}

fn proposal_id(path: Result<Path<Uuid>, PathRejection>) -> Result<ProposalId, ApiError> {
    let Path(id) = path?;
    Ok(ProposalId::from_uuid(id))
}

/// `GET /api/v1/proposals`.
#[utoipa::path(
    get, path = "/api/v1/proposals", tag = "proposals",
    params(("status" = Option<String>, Query), ("cursor" = Option<String>, Query),
           ("limit" = Option<usize>, Query)),
    responses((status = 200, body = Page<ProposalDto>),
              (status = 503, description = "runtime_unavailable")),
    security(("bearer" = []))
)]
pub async fn list(
    State(state): State<AppState>,
    query: Result<Query<ProposalsQuery>, QueryRejection>,
) -> Result<Json<Page<ProposalDto>>, ApiError> {
    let Query(mut query) = query?;
    query.limit = clamp_limit(query.limit);
    Ok(Json(state.evaluator()?.proposals(&query).await?))
}

/// `POST /api/v1/proposals/{proposal_id}/accept`.
#[utoipa::path(
    post, path = "/api/v1/proposals/{proposal_id}/accept", tag = "proposals",
    params(("proposal_id" = String, Path)), request_body = ProposalDecisionRequest,
    responses((status = 200, body = ProposalDto), (status = 404, description = "proposal_not_found")),
    security(("bearer" = []))
)]
pub async fn accept(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ProposalDto>, ApiError> {
    decide(state, path, &headers, &body, true).await
}

/// `POST /api/v1/proposals/{proposal_id}/reject`.
#[utoipa::path(
    post, path = "/api/v1/proposals/{proposal_id}/reject", tag = "proposals",
    params(("proposal_id" = String, Path)), request_body = ProposalDecisionRequest,
    responses((status = 200, body = ProposalDto), (status = 404, description = "proposal_not_found")),
    security(("bearer" = []))
)]
pub async fn reject(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ProposalDto>, ApiError> {
    decide(state, path, &headers, &body, false).await
}

async fn decide(
    state: AppState,
    path: Result<Path<Uuid>, PathRejection>,
    headers: &HeaderMap,
    body: &[u8],
    accept: bool,
) -> Result<Json<ProposalDto>, ApiError> {
    let id = proposal_id(path)?;
    let key = idempotency_key(headers)?;
    let request: ProposalDecisionRequest = super::parse_body(body)?;
    let ctx = command_ctx(key.as_deref());
    Ok(Json(
        state
            .evaluator()?
            .decide_proposal(id, accept, request.note, ctx)
            .await?,
    ))
}
