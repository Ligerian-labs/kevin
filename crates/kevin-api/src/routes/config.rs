//! `/api/v1/config` — the effective configuration, redacted.
//!
//! Loopback only: even though the response never contains a secret value
//! (`kevin_config::Resolved::redacted_json` replaces every `*token*`/`*key*`
//! leaf with `***` and masks URL passwords), it does describe the whole
//! deployment, so it is not exposed to a remote caller (plan/09 T5).

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::dto::ConfigDto;
use crate::error::ApiError;
use crate::routes::Peer;
use crate::state::AppState;

/// Routes of this module.
pub fn router() -> Router<AppState> {
    Router::new().route("/config", get(effective_config))
}

/// `GET /api/v1/config`.
#[utoipa::path(
    get, path = "/api/v1/config", tag = "config",
    responses((status = 200, body = ConfigDto),
              (status = 403, description = "forbidden (loopback-only endpoint)")),
    security(("bearer" = []))
)]
pub async fn effective_config(
    State(state): State<AppState>,
    peer: Peer,
) -> Result<Json<ConfigDto>, ApiError> {
    peer.require_loopback()?;
    let json = state.config().redacted_json();
    let dto: ConfigDto = serde_json::from_value(json)
        .map_err(|e| ApiError::internal(format!("configuration is not serialisable: {e}")))?;
    Ok(Json(dto))
}
