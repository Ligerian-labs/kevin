//! The three SSE endpoints (`plan/07-api-and-tui.md` §Event streams):
//! `GET /api/v1/runs/{run_id}/events`, `GET /api/v1/events` and
//! `GET /api/v1/tasks/{task_id}/log/stream`.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::response::sse::Event as SseEvent;
use axum::routing::get;
use futures::Stream;
use uuid::Uuid;

use crate::dto::{EventStreamQuery, TaskLogQueryDto};
use crate::error::ApiError;
use crate::routes::{rate_key, tasks::task_id};
use crate::sse::{self, EventFilter, Start};
use crate::state::AppState;

/// How often the task-log stream polls `orch.task_log` for new lines. Log
/// lines are deliberately **not** on the event bus (plan/07: volume), so this
/// endpoint tails the projection instead.
const LOG_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", get(firehose))
        .route("/runs/{run_id}/events", get(run_events))
        .route("/tasks/{task_id}/log/stream", get(task_log_stream))
}

fn last_event_id(headers: &HeaderMap) -> Option<&str> {
    headers.get("last-event-id").and_then(|v| v.to_str().ok())
}

/// `GET /api/v1/events` — every event, optionally filtered by `?types=`.
#[utoipa::path(
    get, path = "/api/v1/events", tag = "events",
    params(("types" = Option<String>, Query, description = "`run.*,task.*` filter"),
           ("from" = Option<u64>, Query, description = "Replay from this position"),
           ("Last-Event-ID" = Option<u64>, Header, description = "Resume after this position")),
    responses((status = 200, description = "text/event-stream of EventDto",
               content_type = "text/event-stream")),
    security(("bearer" = []))
)]
pub async fn firehose(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<EventStreamQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query?;
    let permit = state.sse_gate().acquire(&rate_key(&headers))?;
    let start = Start::resolve(last_event_id(&headers), query.from);
    let filter = EventFilter {
        run_id: None,
        types: EventFilter::parse_types(query.types.as_deref()),
    };
    let stream = sse::event_stream(state.events().clone(), start, filter, None, permit);
    Ok(sse::respond(stream, state.server().sse_keepalive))
}

/// `GET /api/v1/runs/{run_id}/events` — the events of one run.
#[utoipa::path(
    get, path = "/api/v1/runs/{run_id}/events", tag = "events",
    params(("run_id" = String, Path), ("types" = Option<String>, Query),
           ("from" = Option<u64>, Query),
           ("Last-Event-ID" = Option<u64>, Header)),
    responses((status = 200, description = "text/event-stream of EventDto",
               content_type = "text/event-stream"),
              (status = 404, description = "run_not_found")),
    security(("bearer" = []))
)]
pub async fn run_events(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    query: Result<Query<EventStreamQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let run_id = super::runs::run_id(path)?;
    let Query(query) = query?;
    let run = state
        .read()
        .run(run_id)
        .await?
        .ok_or_else(|| ApiError::run_not_found(run_id.as_uuid()))?;

    let permit = state.sse_gate().acquire(&rate_key(&headers))?;
    let start = Start::resolve(last_event_id(&headers), query.from);
    let filter = EventFilter {
        run_id: Some(run_id.as_uuid()),
        types: EventFilter::parse_types(query.types.as_deref()),
    };
    // A live-only connection opens with the current `RunDto` so the client
    // never needs a second request to know where it stands.
    let snapshot = (start == Start::Live)
        .then(|| serde_json::to_value(&run).unwrap_or(serde_json::Value::Null));
    let stream = sse::event_stream(state.events().clone(), start, filter, snapshot, permit);
    Ok(sse::respond(stream, state.server().sse_keepalive))
}

/// `GET /api/v1/tasks/{task_id}/log/stream` — the transcript, tailed.
///
/// `Last-Event-ID` is the log `seq`, not a global position (plan/07 §Event
/// streams).
#[utoipa::path(
    get, path = "/api/v1/tasks/{task_id}/log/stream", tag = "events",
    params(("task_id" = String, Path), ("attempt" = Option<u8>, Query),
           ("after_seq" = Option<u64>, Query),
           ("Last-Event-ID" = Option<u64>, Header, description = "Resume after this log seq")),
    responses((status = 200, description = "text/event-stream of TaskLogLineDto",
               content_type = "text/event-stream")),
    security(("bearer" = []))
)]
pub async fn task_log_stream(
    State(state): State<AppState>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    query: Result<Query<TaskLogQueryDto>, QueryRejection>,
) -> Result<Response, ApiError> {
    let id = task_id(path)?;
    let Query(query) = query?;
    let permit = state.sse_gate().acquire(&rate_key(&headers))?;
    let after = last_event_id(&headers)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .or(query.after_seq)
        .unwrap_or(0);
    let stream = log_stream(state.clone(), id, query.attempt, after, permit);
    Ok(sse::respond(stream, state.server().sse_keepalive))
}

struct LogState {
    state: AppState,
    task_id: kevin_domain::ids::TaskId,
    attempt: Option<u8>,
    after: u64,
    pending: std::collections::VecDeque<crate::dto::TaskLogLineDto>,
    _permit: crate::state::SsePermit,
}

fn log_stream(
    state: AppState,
    task_id: kevin_domain::ids::TaskId,
    attempt: Option<u8>,
    after: u64,
    permit: crate::state::SsePermit,
) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    futures::stream::unfold(
        LogState {
            state,
            task_id,
            attempt,
            after,
            pending: std::collections::VecDeque::new(),
            _permit: permit,
        },
        |mut state| async move {
            loop {
                if let Some(line) = state.pending.pop_front() {
                    state.after = state.after.max(line.seq);
                    return Some((Ok(sse::data_event(line.seq, "task.log", &line)), state));
                }
                let query = TaskLogQueryDto {
                    attempt: state.attempt,
                    after_seq: Some(state.after),
                    limit: Some(200),
                };
                match state.state.read().task_log(state.task_id, &query).await {
                    Ok(page) if page.items.is_empty() => {
                        tokio::time::sleep(LOG_POLL_INTERVAL).await;
                    }
                    Ok(page) => state.pending.extend(page.items),
                    Err(err) => {
                        tracing::warn!(error = %err, "task log stream failed");
                        return None;
                    }
                }
            }
        },
    )
}
