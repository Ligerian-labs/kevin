//! `/api/v1/memory` and `/api/v1/lessons` — retrieval memory (`kevin-memory`).

use axum::Json;
use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use kevin_domain::ids::MemoryItemId;
use uuid::Uuid;

use crate::dto::{LessonsQuery, MemoryItemDto, MemorySearchQuery, Page};
use crate::error::ApiError;
use crate::routes::{api_actor, clamp_limit};
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memory/search", get(search))
        .route("/lessons", get(lessons))
        .route("/memory/{item_id}", delete(forget))
}

/// `GET /api/v1/memory/search`.
#[utoipa::path(
    get, path = "/api/v1/memory/search", tag = "memory",
    params(("q" = String, Query, description = "Query text"),
           ("kinds" = Option<String>, Query, description = "Comma-separated memory kinds"),
           ("top_k" = Option<usize>, Query)),
    responses((status = 200, body = Vec<MemoryItemDto>),
              (status = 400, description = "invalid_request")),
    security(("bearer" = []))
)]
pub async fn search(
    State(state): State<AppState>,
    query: Result<Query<MemorySearchQuery>, QueryRejection>,
) -> Result<Json<Vec<MemoryItemDto>>, ApiError> {
    let Query(mut query) = query?;
    if query.q.trim().is_empty() {
        return Err(ApiError::invalid_request("`q` must not be empty"));
    }
    query.top_k = clamp_limit(query.top_k);
    Ok(Json(state.memory()?.search(&query).await?))
}

/// `GET /api/v1/lessons`.
#[utoipa::path(
    get, path = "/api/v1/lessons", tag = "memory",
    params(("cursor" = Option<String>, Query), ("limit" = Option<usize>, Query)),
    responses((status = 200, body = Page<MemoryItemDto>)),
    security(("bearer" = []))
)]
pub async fn lessons(
    State(state): State<AppState>,
    query: Result<Query<LessonsQuery>, QueryRejection>,
) -> Result<Json<Page<MemoryItemDto>>, ApiError> {
    let Query(mut query) = query?;
    query.limit = clamp_limit(query.limit);
    Ok(Json(state.memory()?.lessons(&query).await?))
}

/// `DELETE /api/v1/memory/{item_id}` → `ForgetMemoryItem`.
#[utoipa::path(
    delete, path = "/api/v1/memory/{item_id}", tag = "memory",
    params(("item_id" = String, Path)),
    responses((status = 204, description = "The item was forgotten")),
    security(("bearer" = []))
)]
pub async fn forget(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, ApiError> {
    let Path(id) = path?;
    state
        .memory()?
        .forget(MemoryItemId::from_uuid(id), api_actor())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
