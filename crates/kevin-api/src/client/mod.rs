//! The typed HTTP client (`plan/07-api-and-tui.md` §2).
//!
//! `KevinClient` is what `kevin-cli` and `kevin-tui` talk to; they never touch
//! the store. It is behind the `client` feature and pulls neither axum nor the
//! orchestrator, so the TUI stays light.
//!
//! Streams auto-reconnect with exponential backoff (250 ms → 10 s, jittered)
//! and resend the last position as `Last-Event-ID`; a server-side `resync`
//! surfaces as [`ClientError::Resync`] so the consumer refetches a snapshot,
//! and the stream then keeps going from the last position it saw.

/// The shared `text/event-stream` decoder (see [`crate::sse_wire`]).
pub use crate::sse_wire as sse;

use std::time::Duration;

use futures::Stream;
use futures::stream::StreamExt;
use kevin_domain::ids::{MemoryItemId, ProposalId, QuestionId, RunId, TaskId};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::dto::{
    AnswerRequest, ArtifactDto, CancelRunRequest, CostQueryDto, CostReportDto, CreateRunRequest,
    DrainStatusDto, EventDto, LessonsQuery, ListRunsQuery, MemoryItemDto, MemorySearchQuery, Page,
    ProposalDecisionRequest, ProposalDto, ProposalsQuery, QuestionDto, QuestionsQuery, ReadyDto,
    RejectPlanRequest, RetryTaskRequest, RouteScoreDto, RunDto, RunSummaryDto, TaskDto,
    TaskLogLineDto, TaskLogQueryDto, WorkerDoctorDto,
};
use crate::error::ErrorBody;

/// First reconnect delay.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// Longest reconnect delay.
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Everything a client call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered with the error envelope.
    #[error("{code} ({status}): {message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Stable code from `plan/07` §Conventions.
        code: String,
        /// Human-readable, redacted message.
        message: String,
        /// Structured context.
        details: Option<Value>,
    },

    /// The request never reached the server, or the response was malformed.
    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    /// An event stream broke in a way the client could not decode.
    #[error("event stream: {0}")]
    Stream(String),

    /// The server dropped events: refetch a snapshot and keep listening.
    #[error("the stream lagged; refetch a snapshot")]
    Resync,

    /// The base URL or a path could not be built.
    #[error("invalid URL: {0}")]
    Url(String),
}

impl ClientError {
    /// The stable error code, when the failure came from the server.
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            ClientError::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    /// The HTTP status, when the failure came from the server.
    #[must_use]
    pub const fn status(&self) -> Option<u16> {
        match self {
            ClientError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// A typed client for one Kevin daemon.
#[derive(Debug, Clone)]
pub struct KevinClient {
    base: Url,
    token: SecretString,
    http: reqwest::Client,
}

impl KevinClient {
    /// A client for `base` (`http://127.0.0.1:7777`) using `token`.
    pub fn new(base: Url, token: SecretString) -> Self {
        Self::with_http(base, token, reqwest::Client::new())
    }

    /// [`KevinClient::new`] with a caller-provided `reqwest` client (proxy,
    /// timeouts, a custom root store…).
    pub fn with_http(base: Url, token: SecretString, http: reqwest::Client) -> Self {
        Self { base, token, http }
    }

    /// Parses `base` and builds a client.
    pub fn connect(base: &str, token: SecretString) -> Result<Self, ClientError> {
        let base = Url::parse(base).map_err(|e| ClientError::Url(e.to_string()))?;
        Ok(Self::new(base, token))
    }

    /// The server this client talks to.
    #[must_use]
    pub const fn base_url(&self) -> &Url {
        &self.base
    }

    // -- plumbing -----------------------------------------------------------

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url, ClientError> {
        let mut url = self
            .base
            .join(path)
            .map_err(|e| ClientError::Url(format!("{path}: {e}")))?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    fn headers(&self, idempotency: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(value) = HeaderValue::from_str(&format!("Bearer {}", self.token.expose_secret()))
        {
            let mut value = value;
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        if let Some(key) = idempotency
            && let Ok(value) = HeaderValue::from_str(key)
        {
            headers.insert("idempotency-key", value);
        }
        headers
    }

    async fn send<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<&impl Serialize>,
        idempotency: Option<&str>,
    ) -> Result<T, ClientError> {
        let mut request = self
            .http
            .request(method, url)
            .headers(self.headers(idempotency));
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
                .json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if status.is_success() {
            if bytes.is_empty() && status == StatusCode::NO_CONTENT {
                // `DELETE …/memory/{id}` returns no body; `T` is `()` there.
                return serde_json::from_str("null")
                    .map_err(|e| ClientError::Stream(e.to_string()));
            }
            return serde_json::from_slice(&bytes).map_err(|e| {
                ClientError::Stream(format!("could not decode a {status} response: {e}"))
            });
        }
        Err(api_error(status.as_u16(), &bytes))
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ClientError> {
        let url = self.url(path, query)?;
        self.send::<T>(Method::GET, url, None::<&()>, None).await
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        idempotency: Option<&str>,
    ) -> Result<T, ClientError> {
        let url = self.url(path, &[])?;
        self.send::<T>(Method::POST, url, Some(body), idempotency)
            .await
    }

    // -- runs ---------------------------------------------------------------

    /// `POST /api/v1/runs`.
    pub async fn create_run(
        &self,
        request: CreateRunRequest,
        idempotency: Option<&str>,
    ) -> Result<RunDto, ClientError> {
        self.post("api/v1/runs", &request, idempotency).await
    }

    /// `GET /api/v1/runs/{id}`.
    pub async fn get_run(&self, id: RunId) -> Result<RunDto, ClientError> {
        self.get(&format!("api/v1/runs/{id}"), &[]).await
    }

    /// `GET /api/v1/runs`.
    pub async fn list_runs(
        &self,
        query: &ListRunsQuery,
    ) -> Result<Page<RunSummaryDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "status", query.status.clone());
        push(&mut params, "cursor", query.cursor.clone());
        push(&mut params, "limit", query.limit.map(|v| v.to_string()));
        self.get("api/v1/runs", &params).await
    }

    /// `POST /api/v1/runs/{id}/cancel`.
    pub async fn cancel_run(
        &self,
        id: RunId,
        reason: Option<String>,
    ) -> Result<RunDto, ClientError> {
        self.post(
            &format!("api/v1/runs/{id}/cancel"),
            &CancelRunRequest { reason },
            None,
        )
        .await
    }

    /// `POST /api/v1/runs/{id}/plan/approve`.
    pub async fn approve_plan(
        &self,
        id: RunId,
        idempotency: Option<&str>,
    ) -> Result<RunDto, ClientError> {
        self.post(
            &format!("api/v1/runs/{id}/plan/approve"),
            &serde_json::json!({}),
            idempotency,
        )
        .await
    }

    /// `POST /api/v1/runs/{id}/plan/reject`.
    pub async fn reject_plan(&self, id: RunId, feedback: String) -> Result<RunDto, ClientError> {
        self.post(
            &format!("api/v1/runs/{id}/plan/reject"),
            &RejectPlanRequest { feedback },
            None,
        )
        .await
    }

    /// `POST /api/v1/runs/{id}/evaluate`.
    pub async fn evaluate_run(&self, id: RunId) -> Result<(), ClientError> {
        let url = self.url(&format!("api/v1/runs/{id}/evaluate"), &[])?;
        let response = self
            .http
            .post(url)
            .headers(self.headers(None))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = response.bytes().await?;
        Err(api_error(status.as_u16(), &bytes))
    }

    /// `GET /api/v1/runs/{id}/tasks`.
    pub async fn run_tasks(&self, id: RunId) -> Result<Vec<TaskDto>, ClientError> {
        self.get(&format!("api/v1/runs/{id}/tasks"), &[]).await
    }

    // -- tasks --------------------------------------------------------------

    /// `GET /api/v1/tasks/{id}`.
    pub async fn get_task(&self, id: TaskId) -> Result<TaskDto, ClientError> {
        self.get(&format!("api/v1/tasks/{id}"), &[]).await
    }

    /// `POST /api/v1/tasks/{id}/retry`.
    pub async fn retry_task(
        &self,
        id: TaskId,
        exclude_route: bool,
    ) -> Result<TaskDto, ClientError> {
        self.post(
            &format!("api/v1/tasks/{id}/retry"),
            &RetryTaskRequest { exclude_route },
            None,
        )
        .await
    }

    /// `POST /api/v1/tasks/{id}/cancel`.
    pub async fn cancel_task(&self, id: TaskId) -> Result<TaskDto, ClientError> {
        self.post(
            &format!("api/v1/tasks/{id}/cancel"),
            &serde_json::json!({}),
            None,
        )
        .await
    }

    /// `GET /api/v1/tasks/{id}/log`.
    pub async fn task_log(
        &self,
        id: TaskId,
        query: &TaskLogQueryDto,
    ) -> Result<Page<TaskLogLineDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "attempt", query.attempt.map(|v| v.to_string()));
        push(
            &mut params,
            "after_seq",
            query.after_seq.map(|v| v.to_string()),
        );
        push(&mut params, "limit", query.limit.map(|v| v.to_string()));
        self.get(&format!("api/v1/tasks/{id}/log"), &params).await
    }

    /// `GET /api/v1/tasks/{id}/artifacts`.
    pub async fn task_artifacts(&self, id: TaskId) -> Result<Vec<ArtifactDto>, ClientError> {
        self.get(&format!("api/v1/tasks/{id}/artifacts"), &[]).await
    }

    // -- questions ----------------------------------------------------------

    /// `GET /api/v1/questions`.
    pub async fn questions(
        &self,
        query: &QuestionsQuery,
    ) -> Result<Page<QuestionDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "status", query.status.clone());
        push(&mut params, "run_id", query.run_id.map(|id| id.to_string()));
        push(&mut params, "cursor", query.cursor.clone());
        push(&mut params, "limit", query.limit.map(|v| v.to_string()));
        self.get("api/v1/questions", &params).await
    }

    /// `GET /api/v1/questions/{id}`.
    pub async fn get_question(&self, id: QuestionId) -> Result<QuestionDto, ClientError> {
        self.get(&format!("api/v1/questions/{id}"), &[]).await
    }

    /// `POST /api/v1/questions/{id}/answer`.
    pub async fn answer_question(
        &self,
        id: QuestionId,
        answer: AnswerRequest,
        idempotency: Option<&str>,
    ) -> Result<QuestionDto, ClientError> {
        self.post(
            &format!("api/v1/questions/{id}/answer"),
            &answer,
            idempotency,
        )
        .await
    }

    // -- reporting ----------------------------------------------------------

    /// `GET /api/v1/cost`.
    pub async fn cost(&self, query: &CostQueryDto) -> Result<CostReportDto, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "since", query.since.map(|at| at.to_rfc3339()));
        push(&mut params, "run_id", query.run_id.map(|id| id.to_string()));
        push(&mut params, "group_by", query.group_by.clone());
        self.get("api/v1/cost", &params).await
    }

    /// `GET /api/v1/routes`.
    pub async fn routes(&self, kind: Option<&str>) -> Result<Vec<RouteScoreDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "kind", kind.map(ToOwned::to_owned));
        self.get("api/v1/routes", &params).await
    }

    /// `GET /api/v1/memory/search`.
    pub async fn memory_search(
        &self,
        query: &MemorySearchQuery,
    ) -> Result<Vec<MemoryItemDto>, ClientError> {
        let mut params = vec![("q", query.q.clone())];
        push(&mut params, "kinds", query.kinds.clone());
        push(&mut params, "top_k", query.top_k.map(|v| v.to_string()));
        self.get("api/v1/memory/search", &params).await
    }

    /// `GET /api/v1/lessons`.
    pub async fn lessons(&self, query: &LessonsQuery) -> Result<Page<MemoryItemDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "cursor", query.cursor.clone());
        push(&mut params, "limit", query.limit.map(|v| v.to_string()));
        self.get("api/v1/lessons", &params).await
    }

    /// `DELETE /api/v1/memory/{id}`.
    pub async fn forget_memory(&self, id: MemoryItemId) -> Result<(), ClientError> {
        let url = self.url(&format!("api/v1/memory/{id}"), &[])?;
        let response = self
            .http
            .delete(url)
            .headers(self.headers(None))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let bytes = response.bytes().await?;
        Err(api_error(status.as_u16(), &bytes))
    }

    /// `GET /api/v1/proposals`.
    pub async fn proposals(
        &self,
        query: &ProposalsQuery,
    ) -> Result<Page<ProposalDto>, ClientError> {
        let mut params = Vec::new();
        push(&mut params, "status", query.status.clone());
        push(&mut params, "cursor", query.cursor.clone());
        push(&mut params, "limit", query.limit.map(|v| v.to_string()));
        self.get("api/v1/proposals", &params).await
    }

    /// `POST /api/v1/proposals/{id}/accept`.
    pub async fn accept_proposal(
        &self,
        id: ProposalId,
        note: Option<String>,
    ) -> Result<ProposalDto, ClientError> {
        self.post(
            &format!("api/v1/proposals/{id}/accept"),
            &ProposalDecisionRequest { note },
            None,
        )
        .await
    }

    /// `POST /api/v1/proposals/{id}/reject`.
    pub async fn reject_proposal(
        &self,
        id: ProposalId,
        note: Option<String>,
    ) -> Result<ProposalDto, ClientError> {
        self.post(
            &format!("api/v1/proposals/{id}/reject"),
            &ProposalDecisionRequest { note },
            None,
        )
        .await
    }

    /// `GET /api/v1/workers`.
    pub async fn workers(&self) -> Result<Vec<WorkerDoctorDto>, ClientError> {
        self.get("api/v1/workers", &[]).await
    }

    // -- operations ---------------------------------------------------------

    /// `POST`/`DELETE /api/v1/maintenance/drain`.
    pub async fn drain(&self, on: bool) -> Result<DrainStatusDto, ClientError> {
        let url = self.url("api/v1/maintenance/drain", &[])?;
        let method = if on { Method::POST } else { Method::DELETE };
        self.send::<DrainStatusDto>(method, url, None::<&()>, None)
            .await
    }

    /// `GET /api/v1/maintenance/drain`.
    pub async fn drain_status(&self) -> Result<DrainStatusDto, ClientError> {
        self.get("api/v1/maintenance/drain", &[]).await
    }

    /// `GET /readyz`. A `503` is a *readiness answer*, not an error.
    pub async fn ready(&self) -> Result<ReadyDto, ClientError> {
        let url = self.url("readyz", &[])?;
        let response = self.http.get(url).send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if status.is_success() || status == StatusCode::SERVICE_UNAVAILABLE {
            return serde_json::from_slice(&bytes)
                .map_err(|e| ClientError::Stream(format!("could not decode /readyz: {e}")));
        }
        Err(api_error(status.as_u16(), &bytes))
    }

    /// `GET /api/v1/openapi.json`.
    pub async fn openapi(&self) -> Result<Value, ClientError> {
        self.get("api/v1/openapi.json", &[]).await
    }

    // -- streams ------------------------------------------------------------

    /// `GET /api/v1/runs/{id}/events`, reconnecting on its own.
    pub fn run_events(
        &self,
        id: RunId,
        from: Option<u64>,
    ) -> impl Stream<Item = Result<EventDto, ClientError>> + Send + 'static {
        self.event_stream(format!("api/v1/runs/{id}/events"), Vec::new(), from)
    }

    /// `GET /api/v1/events`, reconnecting on its own.
    pub fn events(
        &self,
        types: Option<&str>,
        from: Option<u64>,
    ) -> impl Stream<Item = Result<EventDto, ClientError>> + Send + 'static {
        let mut query = Vec::new();
        push(&mut query, "types", types.map(ToOwned::to_owned));
        self.event_stream("api/v1/events".to_owned(), query, from)
    }

    /// `GET /api/v1/tasks/{id}/log/stream`, reconnecting on its own.
    /// `Last-Event-ID` is the log `seq` here, not a global position.
    pub fn task_log_stream(
        &self,
        id: TaskId,
        after_seq: Option<u64>,
    ) -> impl Stream<Item = Result<TaskLogLineDto, ClientError>> + Send + 'static {
        stream_of(
            self.clone(),
            format!("api/v1/tasks/{id}/log/stream"),
            Vec::new(),
            after_seq,
        )
    }

    fn event_stream(
        &self,
        path: String,
        query: Vec<(&'static str, String)>,
        from: Option<u64>,
    ) -> impl Stream<Item = Result<EventDto, ClientError>> + Send + 'static {
        stream_of::<EventDto>(self.clone(), path, query, from)
    }
}

fn stream_of<T: DeserializeOwned + Send + 'static>(
    client: KevinClient,
    path: String,
    query: Vec<(&'static str, String)>,
    from: Option<u64>,
) -> impl Stream<Item = Result<T, ClientError>> + Send + 'static {
    let state = ReconnectState {
        client,
        path,
        query,
        last_id: from,
        attempt: 0,
        decoder: sse::Decoder::new(),
        body: None,
        pending: std::collections::VecDeque::new(),
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.pending.pop_front() {
                return Some((item, state));
            }
            match state.step().await {
                Ok(()) => {}
                Err(err) => {
                    state.body = None;
                    return Some((Err(err), state));
                }
            }
        }
    })
}

struct ReconnectState<T> {
    client: KevinClient,
    path: String,
    query: Vec<(&'static str, String)>,
    last_id: Option<u64>,
    attempt: u32,
    decoder: sse::Decoder,
    body: Option<std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>>,
    pending: std::collections::VecDeque<Result<T, ClientError>>,
}

impl<T: DeserializeOwned> ReconnectState<T> {
    async fn step(&mut self) -> Result<(), ClientError> {
        if self.body.is_none() {
            self.connect().await?;
            return Ok(());
        }
        let Some(body) = self.body.as_mut() else {
            return Ok(());
        };
        match body.next().await {
            None => {
                // The server closed the stream: reconnect from where we are.
                self.body = None;
                Ok(())
            }
            Some(Err(err)) => {
                self.body = None;
                Err(ClientError::Transport(err))
            }
            Some(Ok(chunk)) => {
                for message in self.decoder.push(&chunk) {
                    if let Some(id) = message.id.as_deref().and_then(|v| v.parse::<u64>().ok()) {
                        self.last_id = Some(id);
                    }
                    match message.name() {
                        crate::dto::SSE_RESYNC => self.pending.push_back(Err(ClientError::Resync)),
                        crate::dto::SSE_SNAPSHOT => {}
                        _ => match serde_json::from_str::<T>(&message.data) {
                            Ok(value) => self.pending.push_back(Ok(value)),
                            Err(err) => self.pending.push_back(Err(ClientError::Stream(format!(
                                "undecodable `{}` payload: {err}",
                                message.name()
                            )))),
                        },
                    }
                }
                Ok(())
            }
        }
    }

    async fn connect(&mut self) -> Result<(), ClientError> {
        if self.attempt > 0 {
            tokio::time::sleep(backoff(self.attempt)).await;
        }
        self.attempt += 1;
        self.decoder = sse::Decoder::new();

        let url = self.client.url(&self.path, &self.query)?;
        let mut request = self
            .client
            .http
            .get(url)
            .headers(self.client.headers(None))
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if let Some(last) = self.last_id
            && let Ok(value) = HeaderValue::from_str(&last.to_string())
        {
            request = request.header("last-event-id", value);
        }

        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let bytes = response.bytes().await?;
            return Err(api_error(status.as_u16(), &bytes));
        }
        self.attempt = 0;
        self.body = Some(Box::pin(response.bytes_stream()));
        Ok(())
    }
}

/// 250 ms → 10 s, doubling, with up to 25 % jitter.
fn backoff(attempt: u32) -> Duration {
    let exponential = BACKOFF_MIN.saturating_mul(1u32 << attempt.min(6));
    let capped = exponential.min(BACKOFF_MAX);
    let jitter_range = u64::try_from(capped.as_millis()).unwrap_or(u64::MAX) / 4;
    if jitter_range == 0 {
        return capped;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    capped + Duration::from_millis(nanos % jitter_range)
}

fn api_error(status: u16, body: &[u8]) -> ClientError {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(envelope) => ClientError::Api {
            status,
            code: envelope.code,
            message: envelope.message,
            details: envelope.details,
        },
        Err(_) => ClientError::Api {
            status,
            code: "internal".to_owned(),
            message: String::from_utf8_lossy(body).chars().take(512).collect(),
            details: None,
        },
    }
}

fn push(params: &mut Vec<(&'static str, String)>, key: &'static str, value: Option<String>) {
    if let Some(value) = value {
        params.push((key, value));
    }
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_MAX, BACKOFF_MIN, backoff};

    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff(1) >= BACKOFF_MIN);
        for attempt in 0..20 {
            let delay = backoff(attempt);
            assert!(
                delay <= BACKOFF_MAX + BACKOFF_MAX / 4,
                "attempt {attempt} waited {delay:?}"
            );
        }
        assert!(backoff(5) > backoff(1), "the delay grows with attempts");
    }
}
