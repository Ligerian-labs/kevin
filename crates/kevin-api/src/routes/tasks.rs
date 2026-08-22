//! `/api/v1/tasks` — inspect, retry, cancel, transcript and artifacts
//! (`plan/07-api-and-tui.md` §Endpoints).

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use kevin_domain::ids::{ArtifactId, TaskId};
use uuid::Uuid;

use crate::dto::{
    ArtifactDto, EmptyRequest, Page, RetryTaskRequest, TaskDto, TaskLogLineDto, TaskLogQueryDto,
};
use crate::error::ApiError;
use crate::routes::{clamp_limit, idempotency_key, idempotent};
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tasks/{task_id}", get(get_task))
        .route("/tasks/{task_id}/retry", post(retry_task))
        .route("/tasks/{task_id}/cancel", post(cancel_task))
        .route("/tasks/{task_id}/log", get(task_log))
        .route("/tasks/{task_id}/artifacts", get(task_artifacts))
        .route("/artifacts/{artifact_id}", get(artifact_bytes))
}

pub(crate) fn task_id(path: Result<Path<Uuid>, PathRejection>) -> Result<TaskId, ApiError> {
    let Path(id) = path?;
    Ok(TaskId::from_uuid(id))
}

/// `GET /api/v1/tasks/{task_id}`.
#[utoipa::path(
    get, path = "/api/v1/tasks/{task_id}", tag = "tasks",
    params(("task_id" = String, Path)),
    responses((status = 200, body = TaskDto), (status = 404, description = "task_not_found")),
    security(("bearer" = []))
)]
pub async fn get_task(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<TaskDto>, ApiError> {
    let id = task_id(path)?;
    state
        .read()
        .task(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::task_not_found(id.as_uuid()))
}

/// `POST /api/v1/tasks/{task_id}/retry` → `RetryTask`.
#[utoipa::path(
    post, path = "/api/v1/tasks/{task_id}/retry", tag = "tasks",
    params(("task_id" = String, Path)), request_body = RetryTaskRequest,
    responses((status = 202, body = TaskDto), (status = 409, description = "task_not_in_state")),
    security(("bearer" = []))
)]
pub async fn retry_task(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = task_id(path)?;
    let key = idempotency_key(&headers)?;
    let request: RetryTaskRequest = super::parse_body(&body)?;
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::ACCEPTED,
        |ctx| async move { Ok(runtime.retry_task(id, request.exclude_route, ctx).await?) },
    )
    .await
}

/// `POST /api/v1/tasks/{task_id}/cancel` → `CancelTask`.
#[utoipa::path(
    post, path = "/api/v1/tasks/{task_id}/cancel", tag = "tasks",
    params(("task_id" = String, Path)), request_body = EmptyRequest,
    responses((status = 202, body = TaskDto), (status = 404, description = "task_not_found")),
    security(("bearer" = []))
)]
pub async fn cancel_task(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = task_id(path)?;
    let key = idempotency_key(&headers)?;
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::ACCEPTED,
        |ctx| async move { Ok(runtime.cancel_task(id, ctx).await?) },
    )
    .await
}

/// `GET /api/v1/tasks/{task_id}/log`.
#[utoipa::path(
    get, path = "/api/v1/tasks/{task_id}/log", tag = "tasks",
    params(("task_id" = String, Path),
           ("attempt" = Option<u8>, Query), ("after_seq" = Option<u64>, Query),
           ("limit" = Option<usize>, Query)),
    responses((status = 200, body = Page<TaskLogLineDto>)),
    security(("bearer" = []))
)]
pub async fn task_log(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<TaskLogQueryDto>, QueryRejection>,
) -> Result<Json<Page<TaskLogLineDto>>, ApiError> {
    let id = task_id(path)?;
    let Query(mut query) = query?;
    query.limit = clamp_limit(query.limit);
    Ok(Json(state.read().task_log(id, &query).await?))
}

/// `GET /api/v1/tasks/{task_id}/artifacts`.
#[utoipa::path(
    get, path = "/api/v1/tasks/{task_id}/artifacts", tag = "tasks",
    params(("task_id" = String, Path)),
    responses((status = 200, body = Vec<ArtifactDto>)),
    security(("bearer" = []))
)]
pub async fn task_artifacts(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Vec<ArtifactDto>>, ApiError> {
    let id = task_id(path)?;
    Ok(Json(state.read().artifacts_of_task(id).await?))
}

/// `GET /api/v1/artifacts/{artifact_id}` — the bytes, typed by artifact kind.
#[utoipa::path(
    get, path = "/api/v1/artifacts/{artifact_id}", tag = "tasks",
    params(("artifact_id" = String, Path)),
    responses((status = 200, description = "The artifact bytes",
               content_type = "application/octet-stream"),
              (status = 404, description = "artifact_not_found")),
    security(("bearer" = []))
)]
pub async fn artifact_bytes(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Response, ApiError> {
    let Path(id) = path?;
    let id = ArtifactId::from_uuid(id);
    let artifact = state.read().artifact(id).await?.ok_or_else(|| {
        ApiError::new(
            crate::error::ErrorCode::ArtifactNotFound,
            format!("artifact {id} does not exist"),
        )
    })?;
    let (content_type, bytes) = state.artifacts().read(&artifact).await?;
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}
