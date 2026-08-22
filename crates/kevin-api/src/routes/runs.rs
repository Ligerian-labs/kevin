//! `/api/v1/runs` — start, list, inspect, cancel, approve/reject the plan and
//! re-run the judge (`plan/07-api-and-tui.md` §Endpoints).

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use kevin_domain::ids::RunId;
use uuid::Uuid;

use crate::dto::{
    CancelRunRequest, CreateRunRequest, EmptyRequest, ListRunsQuery, Page, RejectPlanRequest,
    RunDto, RunSummaryDto, TaskDto,
};
use crate::error::ApiError;
use crate::routes::{clamp_limit, idempotency_key, idempotent};
use crate::state::{AppState, MAX_GOAL_BYTES};

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/runs", post(create_run).get(list_runs))
        .route("/runs/{run_id}", get(get_run))
        .route("/runs/{run_id}/cancel", post(cancel_run))
        .route("/runs/{run_id}/plan/approve", post(approve_plan))
        .route("/runs/{run_id}/plan/reject", post(reject_plan))
        .route("/runs/{run_id}/evaluate", post(evaluate_run))
        .route("/runs/{run_id}/tasks", get(list_run_tasks))
}

/// The `{run_id}` path segment.
pub(crate) fn run_id(path: Result<Path<Uuid>, PathRejection>) -> Result<RunId, ApiError> {
    let Path(id) = path?;
    Ok(RunId::from_uuid(id))
}

/// `POST /api/v1/runs` → `StartRun`.
#[utoipa::path(
    post, path = "/api/v1/runs", tag = "runs",
    request_body = CreateRunRequest,
    params(("Idempotency-Key" = Option<String>, Header, description = "Replay-safe command id")),
    responses(
        (status = 201, description = "The run was accepted", body = RunDto),
        (status = 200, description = "Idempotent replay of an identical request", body = RunDto),
        (status = 409, description = "The key was used with a different body"),
        (status = 503, description = "The runtime is draining"),
    ),
    security(("bearer" = []))
)]
pub async fn create_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = idempotency_key(&headers)?;
    let request: CreateRunRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid_request(format!("invalid JSON body: {e}")))?;
    validate_create(&request)?;

    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::CREATED,
        |ctx| async move { Ok(runtime.start_run(request, ctx).await?) },
    )
    .await
}

fn validate_create(request: &CreateRunRequest) -> Result<(), ApiError> {
    use crate::error::ErrorCode;
    if request.goal.trim().is_empty() {
        return Err(ApiError::new(ErrorCode::InvalidGoal, "the goal is empty"));
    }
    if request.goal.len() > MAX_GOAL_BYTES {
        return Err(ApiError::new(
            ErrorCode::InvalidGoal,
            format!("the goal is longer than {MAX_GOAL_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// `GET /api/v1/runs`.
#[utoipa::path(
    get, path = "/api/v1/runs", tag = "runs",
    params(("status" = Option<String>, Query, description = "Keep only runs in this status"),
           ("cursor" = Option<String>, Query, description = "Cursor from a previous page"),
           ("limit" = Option<usize>, Query, description = "Page size (max 200)")),
    responses((status = 200, body = Page<RunSummaryDto>)),
    security(("bearer" = []))
)]
pub async fn list_runs(
    State(state): State<AppState>,
    query: Result<Query<ListRunsQuery>, QueryRejection>,
) -> Result<Json<Page<RunSummaryDto>>, ApiError> {
    let Query(mut query) = query?;
    query.limit = clamp_limit(query.limit);
    Ok(Json(state.read().runs(&query).await?))
}

/// `GET /api/v1/runs/{run_id}`.
#[utoipa::path(
    get, path = "/api/v1/runs/{run_id}", tag = "runs",
    params(("run_id" = String, Path)),
    responses((status = 200, body = RunDto), (status = 404, description = "run_not_found")),
    security(("bearer" = []))
)]
pub async fn get_run(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<RunDto>, ApiError> {
    let id = run_id(path)?;
    state
        .read()
        .run(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::run_not_found(id.as_uuid()))
}

/// `POST /api/v1/runs/{run_id}/cancel` → `CancelRun`.
#[utoipa::path(
    post, path = "/api/v1/runs/{run_id}/cancel", tag = "runs",
    params(("run_id" = String, Path)), request_body = CancelRunRequest,
    responses((status = 202, body = RunDto), (status = 404, description = "run_not_found")),
    security(("bearer" = []))
)]
pub async fn cancel_run(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = run_id(path)?;
    let key = idempotency_key(&headers)?;
    let request: CancelRunRequest = super::parse_body(&body)?;
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::ACCEPTED,
        |ctx| async move { Ok(runtime.cancel_run(id, request.reason, ctx).await?) },
    )
    .await
}

/// `POST /api/v1/runs/{run_id}/plan/approve` → `ApprovePlan`.
#[utoipa::path(
    post, path = "/api/v1/runs/{run_id}/plan/approve", tag = "runs",
    params(("run_id" = String, Path)), request_body = EmptyRequest,
    responses((status = 202, body = RunDto), (status = 409, description = "run_not_in_state")),
    security(("bearer" = []))
)]
pub async fn approve_plan(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = run_id(path)?;
    let key = idempotency_key(&headers)?;
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::ACCEPTED,
        |ctx| async move { Ok(runtime.approve_plan(id, ctx).await?) },
    )
    .await
}

/// `POST /api/v1/runs/{run_id}/plan/reject` → `RejectPlan`.
#[utoipa::path(
    post, path = "/api/v1/runs/{run_id}/plan/reject", tag = "runs",
    params(("run_id" = String, Path)), request_body = RejectPlanRequest,
    responses((status = 202, body = RunDto), (status = 400, description = "invalid_request")),
    security(("bearer" = []))
)]
pub async fn reject_plan(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = run_id(path)?;
    let key = idempotency_key(&headers)?;
    let request: RejectPlanRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid_request(format!("`feedback` is required: {e}")))?;
    if request.feedback.trim().is_empty() {
        return Err(ApiError::invalid_request("`feedback` must not be empty"));
    }
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::ACCEPTED,
        |ctx| async move { Ok(runtime.reject_plan(id, request.feedback, ctx).await?) },
    )
    .await
}

/// `POST /api/v1/runs/{run_id}/evaluate` — re-run the judge.
#[utoipa::path(
    post, path = "/api/v1/runs/{run_id}/evaluate", tag = "runs",
    params(("run_id" = String, Path)), request_body = EmptyRequest,
    responses((status = 202, description = "The evaluation was queued")),
    security(("bearer" = []))
)]
pub async fn evaluate_run(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let id = run_id(path)?;
    let key = idempotency_key(&headers)?;
    state
        .runtime()
        .evaluate_run(id, super::command_ctx(key.as_deref()))
        .await?;
    Ok(StatusCode::ACCEPTED)
}

/// `GET /api/v1/runs/{run_id}/tasks`.
#[utoipa::path(
    get, path = "/api/v1/runs/{run_id}/tasks", tag = "tasks",
    params(("run_id" = String, Path)),
    responses((status = 200, body = Vec<TaskDto>), (status = 404, description = "run_not_found")),
    security(("bearer" = []))
)]
pub async fn list_run_tasks(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Vec<TaskDto>>, ApiError> {
    let id = run_id(path)?;
    if state.read().run(id).await?.is_none() {
        return Err(ApiError::run_not_found(id.as_uuid()));
    }
    Ok(Json(state.read().tasks_of_run(id).await?))
}
