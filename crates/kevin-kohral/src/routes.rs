//! The Hermes-dialect HTTP surface (`plan/08-kohral-runtime.md` §1.1).
//!
//! Mounted by `kevin serve --kohral` on `kohral.bind`, next to — never inside
//! — Kevin's own `/api/v1`: the two surfaces have different tokens, different
//! error envelopes and different lifecycles.

use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router, middleware};
use kevin_api::state::Idempotency;
use kevin_domain::run::CancelRun;
use kevin_domain::{Actor, RunId};
use kevin_orchestrator::services::CommandContext;
use kevin_telemetry::events;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{KohralError, KohralErrorCode, KohralResult};
use crate::ledger::{LedgerRow, NewTurn, SessionMessage};
use crate::state::KohralState;
use crate::{attachments, capabilities, catalog, hash, turn};

/// `X-Hermes-Session-Key`.
pub const SESSION_KEY_HEADER: &str = "x-hermes-session-key";
/// `Idempotency-Key`.
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
/// Largest turn body Kevin accepts (the history cap plus room for the rest).
pub const MAX_TURN_BYTES: usize = 1024 * 1024;

/// The complete Kohral router.
///
/// Auth is a middleware over everything but `/health` and `/v1/health`, so a
/// new route is authenticated by default rather than by remembering to add it.
pub fn router(state: KohralState) -> Router {
    let attachment_limit = usize::try_from(state.options().max_attachment_bytes)
        .unwrap_or(usize::MAX)
        .saturating_add(1);

    let attachments = Router::new()
        .route(
            "/v1/attachments/{conversation_id}/{message_id}/{attachment_id}",
            put(attachments::put).delete(attachments::delete),
        )
        .layer(DefaultBodyLimit::max(attachment_limit));

    Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/health/detailed", get(health_detailed))
        .route("/v1/capabilities", get(capabilities_route))
        .route("/v1/kohral/models", get(models))
        .route(
            "/v1/runs",
            post(submit_turn).layer(DefaultBodyLimit::max(MAX_TURN_BYTES)),
        )
        .route("/v1/runs/{run_id}", get(run_status))
        .route("/v1/runs/{run_id}/stop", post(stop_run))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{session_id}", get(session))
        .route("/api/sessions/{session_id}/messages", get(session_messages))
        .route(
            "/v1/maintenance/drain",
            post(begin_drain).get(drain_state).delete(cancel_drain),
        )
        .merge(attachments)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::auth::require_token,
        ))
        .fallback(not_found)
        .with_state(state)
}

async fn not_found() -> Response {
    KohralError::new(KohralErrorCode::InvalidRequest, "no such endpoint").into_response()
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// `GET /health` — Kohral's unauthenticated liveness probe.
async fn health(State(state): State<KohralState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "platform": "kevin",
        "version": state.options().version,
    }))
}

/// `GET /health/detailed` — readiness plus the numeric fields
/// `KevinRuntimeStrategy::metrics()` scrapes.
async fn health_detailed(State(state): State<KohralState>) -> Json<Value> {
    let active = state.ledger().active_runs().await;
    let db_ok = active.is_ok();
    let draining = state.is_draining();
    crate::metrics::draining(draining);
    let active_runs = active.unwrap_or(0);
    // The scrape is also where the in-flight gauge is reconciled with the
    // ledger, so a crash mid-turn cannot skew it forever.
    crate::metrics::active_turns(active_runs);
    Json(json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "platform": "kevin",
        "version": state.options().version,
        "uptime_s": state.uptime_seconds(),
        "active_runs": active_runs,
        "active_actors": state.handle().active_runs(),
        "draining": draining,
        "drainable": draining,
        "accepting": !draining,
        "checks": {"database": db_ok},
    }))
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

async fn capabilities_route(State(state): State<KohralState>) -> Json<Value> {
    Json(capabilities::document(
        &state.options().version,
        state.config().roles.planner.as_str(),
        state.options().temporary_attachments,
    ))
}

async fn models(State(state): State<KohralState>) -> Json<Value> {
    Json(state.runtime_catalog().to_json())
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// `POST /v1/runs` — submit a turn (`plan/08` §1.2).
///
/// The order of the checks is part of the contract:
///
/// 1. a malformed `Idempotency-Key` is a `400` before anything is looked up;
/// 2. a **known** key resolves even while draining — Kohral must be able to
///    finish a turn it already handed over;
/// 3. only then does draining refuse a new key with `503 gateway_draining`.
async fn submit_turn(
    State(state): State<KohralState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    match submit(&state, &headers, &body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn submit(state: &KohralState, headers: &HeaderMap, body: &[u8]) -> KohralResult<Response> {
    let key = header(headers, IDEMPOTENCY_HEADER).unwrap_or_default();
    turn::validate_idempotency_key(&key)?;
    let session_key = header(headers, SESSION_KEY_HEADER).filter(|value| !value.is_empty());

    let raw: Value = serde_json::from_slice(body).map_err(|error| {
        KohralError::new(
            KohralErrorCode::InvalidRequest,
            format!("the request body is not JSON: {error}"),
        )
    })?;
    let request: turn::TurnRequest = serde_json::from_value(raw.clone()).map_err(|error| {
        KohralError::new(
            KohralErrorCode::InvalidRequest,
            format!("the request body is not a turn: {error}"),
        )
    })?;
    let request_hash = hash::canonical_request_hash(&raw, session_key.as_deref());

    // 2. A key we already accepted always answers, draining or not.
    if let Some(existing) = state.ledger().by_key(&key).await? {
        if existing.request_hash != request_hash {
            crate::metrics::turn_conflicted();
            return Err(KohralError::new(
                KohralErrorCode::IdempotencyConflict,
                "this Idempotency-Key was already used with a different request",
            ));
        }
        crate::metrics::turn_replayed();
        return Ok(ok(&existing));
    }

    // 3. New work is refused while draining.
    if state.is_draining() {
        return Err(KohralError::new(
            KohralErrorCode::GatewayDraining,
            "the runtime is draining and does not accept new turns",
        ));
    }

    request.validate()?;
    let resolution = state.runtime_catalog().resolve(&request.model);
    if resolution == catalog::Resolution::Unknown {
        return Err(KohralError::new(
            KohralErrorCode::UnknownModel,
            format!("`{}` is not in this runtime's model catalog", request.model),
        ));
    }

    let run_id = RunId::new();
    let accepted = turn::accept(
        run_id,
        &request,
        &key,
        session_key.as_deref(),
        &resolution,
        &state.config().budget,
        &state.options().environment(),
    );
    let message_id = turn::assistant_message_id(run_id);
    if let Some(alias) = &accepted.model_override {
        // `plan/08` §1.2: applied as a per-run `[roles]` override on the
        // `StartRun` command (`plan/02` §Run), not by mutating configuration —
        // the daemon serves other runs concurrently.
        tracing::info!(
            { kevin_telemetry::fields::KOHRAL_TURN_ID } = %key,
            alias = alias.as_str(),
            "the turn pinned a model; applying it as a per-run role override"
        );
    }

    let new_turn = NewTurn {
        idempotency_key: key.clone(),
        request_hash,
        request_json: json!({"body": raw, "session_key": session_key}),
        run_id: run_id.as_uuid(),
        session_id: accepted.session_id.clone(),
        session_key: session_key.clone(),
        model: Some(request.model.clone()).filter(|model| !model.is_empty()),
        message_id,
    };

    // Acceptance is committed before execution starts (`plan/08` §1.2): the
    // ledger row is the promise, `run.started` is the work. They are two
    // statements rather than one transaction because `EventStore::append`
    // owns its own; the promise is still safe, because the command id is
    // derived from the key, so `core.processed_commands` makes `run.started`
    // exactly-once, and a failure between the two removes the promise again.
    let row = match state.ledger().accept(&new_turn).await? {
        crate::ledger::Accepted::Replay(existing) => {
            crate::metrics::turn_replayed();
            return Ok(ok(&existing));
        }
        crate::ledger::Accepted::Fresh(row) => *row,
    };

    let user_message = SessionMessage {
        message_id: turn::user_message_id(run_id),
        session_id: accepted.session_id.clone(),
        run_id: run_id.as_uuid(),
        role: "user".to_owned(),
        content: request.input.clone(),
        created_at: row.created_at,
    };
    if let Err(error) = state.ledger().record_user_message(&user_message).await {
        tracing::warn!(error = %error, "recording the turn's user message failed");
    }

    let ctx = CommandContext::new(
        Idempotency::command_id(&key),
        Actor::kohral(state.config().kevin.instance_name.clone()),
        run_id,
    );
    if let Err(error) = state.handle().start_run(accepted.command, &ctx).await {
        // Nothing was promised after all: drop the row so Kohral may retry
        // the same turn id.
        let _ = state.ledger().forget(&key).await;
        tracing::error!(error = %error, kohral_turn_id = %key, "starting a Kohral turn failed");
        return Err(KohralError::new(
            KohralErrorCode::StorageUnavailable,
            "the runtime could not accept this turn",
        ));
    }

    tracing::info!(
        { kevin_telemetry::fields::EVENT } = events::kohral::TURN_ACCEPTED,
        { kevin_telemetry::fields::KOHRAL_TURN_ID } = %key,
        run_id = %run_id,
        session_id = %accepted.session_id,
        "Kohral turn accepted"
    );
    crate::metrics::turn_accepted();
    Ok((StatusCode::ACCEPTED, Json(row.status_object())).into_response())
}

/// `GET /v1/runs/{run_id}` — the durable status, always from the ledger.
async fn run_status(
    State(state): State<KohralState>,
    Path(run_id): Path<String>,
) -> KohralResult<Response> {
    let row = lookup(&state, &run_id).await?;
    Ok(ok(&row))
}

/// `POST /v1/runs/{run_id}/stop` — idempotent interrupt (`plan/08` §1.9).
async fn stop_run(
    State(state): State<KohralState>,
    Path(run_id): Path<String>,
) -> KohralResult<Response> {
    let row = lookup(&state, &run_id).await?;
    if row.status.is_terminal() {
        return Ok(ok(&row));
    }
    let id = RunId::from_uuid(row.run_id);
    let stopping = state
        .ledger()
        .mark_stopping(id.as_uuid())
        .await?
        .unwrap_or(row);
    let ctx = CommandContext::new(
        kevin_domain::CommandId::new(),
        Actor::kohral(state.config().kevin.instance_name.clone()),
        id,
    );
    if let Err(error) = state
        .handle()
        .run_service()
        .cancel(
            id,
            CancelRun {
                by: "kohral".to_owned(),
                reason: "the operator stopped this turn".to_owned(),
            },
            &ctx,
        )
        .await
        && !error.is_invalid_transition()
    {
        tracing::warn!(error = %error, run_id = %id, "cancelling a Kohral turn failed");
    }
    Ok(ok(&stopping))
}

async fn lookup(state: &KohralState, run_id: &str) -> KohralResult<LedgerRow> {
    let uuid = Uuid::parse_str(run_id).map_err(|_| run_not_found(run_id))?;
    state
        .ledger()
        .by_run(uuid)
        .await?
        .ok_or_else(|| run_not_found(run_id))
}

fn run_not_found(run_id: &str) -> KohralError {
    KohralError::new(
        KohralErrorCode::RunNotFound,
        format!("no run {run_id} on this runtime"),
    )
}

fn ok(row: &LedgerRow) -> Response {
    (StatusCode::OK, Json(row.status_object())).into_response()
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// Paging parameters Kohral may send.
#[derive(Debug, Clone, serde::Deserialize)]
struct Paging {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

const fn default_limit() -> i64 {
    100
}

/// `GET /api/sessions` — the list envelope Kohral's `sessions()` reads
/// (`payload['sessions'] ?? payload`), with `data` for Hermes parity.
async fn sessions(
    State(state): State<KohralState>,
    Query(paging): Query<Paging>,
) -> KohralResult<Json<Value>> {
    let limit = paging.limit.clamp(1, 500);
    let offset = paging.offset.max(0);
    let sessions = state.ledger().sessions(limit, offset).await?;
    let rows: Vec<Value> = sessions
        .iter()
        .map(crate::ledger::SessionSummary::to_json)
        .collect();
    Ok(Json(json!({
        "object": "list",
        "sessions": rows,
        "data": rows,
        "limit": limit,
        "offset": offset,
        "has_more": i64::try_from(rows.len()).unwrap_or(i64::MAX) == limit,
    })))
}

async fn session(
    State(state): State<KohralState>,
    Path(session_id): Path<String>,
) -> KohralResult<Json<Value>> {
    let summary = state.ledger().session(&session_id).await?.ok_or_else(|| {
        KohralError::new(
            KohralErrorCode::SessionNotFound,
            format!("no session {session_id} on this runtime"),
        )
    })?;
    Ok(Json(summary.to_json()))
}

/// `GET /api/sessions/{id}/messages` — `payload['messages'] ?? payload`.
async fn session_messages(
    State(state): State<KohralState>,
    Path(session_id): Path<String>,
) -> KohralResult<Json<Value>> {
    let messages = state.ledger().messages(&session_id).await?;
    let rows: Vec<Value> = messages.iter().map(SessionMessage::to_json).collect();
    Ok(Json(json!({
        "object": "list",
        "session_id": session_id,
        "messages": rows,
        "data": rows,
    })))
}

// ---------------------------------------------------------------------------
// Drain
// ---------------------------------------------------------------------------

async fn begin_drain(State(state): State<KohralState>) -> KohralResult<Json<Value>> {
    state.handle().drain().await;
    log_drain(true);
    drain_payload(&state).await
}

async fn drain_state(State(state): State<KohralState>) -> KohralResult<Json<Value>> {
    drain_payload(&state).await
}

async fn cancel_drain(State(state): State<KohralState>) -> KohralResult<Json<Value>> {
    state.handle().supervisor().undrain();
    log_drain(false);
    drain_payload(&state).await
}

fn log_drain(draining: bool) {
    tracing::info!(
        { kevin_telemetry::fields::EVENT } = events::kohral::DRAIN_CHANGED,
        draining,
        "Kohral drain state changed"
    );
    crate::metrics::draining(draining);
}

/// `HermesRuntimeStrategy::parseDrainState` requires a boolean `accepting` and
/// an integer `activeWork`, and throws `runtime_protocol_error` otherwise
/// (verified against the Kohral source). `draining` / `active_runs` are the
/// `snake_case` aliases `plan/08` §1.7 documents.
async fn drain_payload(state: &KohralState) -> KohralResult<Json<Value>> {
    let draining = state.is_draining();
    let active = state.ledger().active_runs().await.unwrap_or(0);
    Ok(Json(json!({
        "accepting": !draining,
        "activeWork": active,
        "draining": draining,
        "active_runs": active,
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::ledger::TurnStatus;

    #[test]
    fn kohral_can_map_every_status_kevin_reports() {
        // `HermesRuntimeStrategy::turnStatus()` throws `runtime_protocol_error`
        // on a status it does not know, so this vocabulary is frozen:
        // `queued`, `running|waiting_for_approval|stopping` → running,
        // `completed`, `failed`, `cancelled` → interrupted.
        let statuses: BTreeMap<&str, bool> = TurnStatus::ALL
            .into_iter()
            .map(|status| (status.as_str(), status.is_terminal()))
            .collect();
        assert!(!statuses["queued"]);
        assert!(!statuses["running"]);
        assert!(!statuses["stopping"]);
        assert!(statuses["completed"]);
        assert!(statuses["failed"]);
        assert!(statuses["cancelled"]);
        assert_eq!(statuses.len(), 6);
    }
}
