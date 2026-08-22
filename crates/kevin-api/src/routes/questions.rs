//! `/api/v1/questions` — the clarification inbox and `AnswerQuestion`.

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use kevin_domain::ids::QuestionId;
use uuid::Uuid;

use crate::dto::{AnswerRequest, Page, QuestionDto, QuestionsQuery};
use crate::error::ApiError;
use crate::routes::{clamp_limit, idempotency_key, idempotent};
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/questions", get(list_questions))
        .route("/questions/{question_id}", get(get_question))
        .route("/questions/{question_id}/answer", post(answer_question))
}

fn question_id(path: Result<Path<Uuid>, PathRejection>) -> Result<QuestionId, ApiError> {
    let Path(id) = path?;
    Ok(QuestionId::from_uuid(id))
}

/// `GET /api/v1/questions`.
#[utoipa::path(
    get, path = "/api/v1/questions", tag = "questions",
    params(("status" = Option<String>, Query), ("run_id" = Option<String>, Query),
           ("cursor" = Option<String>, Query), ("limit" = Option<usize>, Query)),
    responses((status = 200, body = Page<QuestionDto>)),
    security(("bearer" = []))
)]
pub async fn list_questions(
    State(state): State<AppState>,
    query: Result<Query<QuestionsQuery>, QueryRejection>,
) -> Result<Json<Page<QuestionDto>>, ApiError> {
    let Query(mut query) = query?;
    query.limit = clamp_limit(query.limit);
    Ok(Json(state.read().questions(&query).await?))
}

/// `GET /api/v1/questions/{question_id}`.
#[utoipa::path(
    get, path = "/api/v1/questions/{question_id}", tag = "questions",
    params(("question_id" = String, Path)),
    responses((status = 200, body = QuestionDto), (status = 404, description = "question_not_found")),
    security(("bearer" = []))
)]
pub async fn get_question(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<QuestionDto>, ApiError> {
    let id = question_id(path)?;
    state
        .read()
        .question(id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::question_not_found(id.as_uuid()))
}

/// `POST /api/v1/questions/{question_id}/answer` → `AnswerQuestion`.
#[utoipa::path(
    post, path = "/api/v1/questions/{question_id}/answer", tag = "questions",
    params(("question_id" = String, Path)), request_body = AnswerRequest,
    responses((status = 200, body = QuestionDto),
              (status = 400, description = "invalid_answer"),
              (status = 409, description = "question_already_answered")),
    security(("bearer" = []))
)]
pub async fn answer_question(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = question_id(path)?;
    let key = idempotency_key(&headers)?;
    let answer: AnswerRequest = serde_json::from_slice(&body)
        .map_err(|e| ApiError::invalid_request(format!("invalid JSON body: {e}")))?;
    if answer.selected.is_empty()
        && answer
            .free_text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
    {
        return Err(ApiError::new(
            crate::error::ErrorCode::InvalidAnswer,
            "an answer needs at least one selected option or some free text",
        ));
    }
    let runtime = state.runtime().clone();
    idempotent(
        &state,
        key.as_deref(),
        &body,
        StatusCode::OK,
        |ctx| async move { Ok(runtime.answer_question(id, answer, ctx).await?) },
    )
    .await
}
