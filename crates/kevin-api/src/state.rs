//! What every handler is given: the ports, the configuration, the token
//! verifier and the process-local admission helpers (idempotency, rate limit,
//! SSE connection cap).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use kevin_config::Resolved;
use kevin_config::schema::Server;

use crate::auth::TokenVerifier;
use crate::error::{ApiError, ErrorCode};
use crate::port::{
    ArtifactsPort, EvaluatorPort, EventsPort, MemoryPort, ReadPort, RouterPort, RuntimePort,
    WorkersPort,
};

/// Per-token request budget: 60 req/s, burst 120 (plan/07 §Limits).
pub const RATE_LIMIT_PER_SECOND: f64 = 60.0;
/// Burst size of the per-token token bucket.
pub const RATE_LIMIT_BURST: f64 = 120.0;
/// SSE connections a single token may hold open (plan/07 §Limits).
pub const MAX_SSE_CONNECTIONS: u32 = 64;
/// Largest request body the API accepts (plan/07 §Limits).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Largest goal text (plan/07 §Limits).
pub const MAX_GOAL_BYTES: usize = 64 * 1024;
/// Largest page a client may ask for.
pub const MAX_PAGE_LIMIT: usize = 200;

/// Everything the handlers need, cloneable (all fields are behind `Arc`).
#[derive(Debug, Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    runtime: Arc<dyn RuntimePort>,
    read: Arc<dyn ReadPort>,
    events: Arc<dyn EventsPort>,
    router: Option<Arc<dyn RouterPort>>,
    evaluator: Option<Arc<dyn EvaluatorPort>>,
    memory: Option<Arc<dyn MemoryPort>>,
    workers: Option<Arc<dyn WorkersPort>>,
    artifacts: Arc<dyn ArtifactsPort>,
    config: Arc<Resolved>,
    server: Server,
    auth: Arc<TokenVerifier>,
    idempotency: Idempotency,
    rate: RateLimiter,
    sse: SseGate,
}

impl AppState {
    /// Starts a builder with the four mandatory ports.
    pub fn builder(
        runtime: Arc<dyn RuntimePort>,
        read: Arc<dyn ReadPort>,
        events: Arc<dyn EventsPort>,
        auth: Arc<TokenVerifier>,
    ) -> AppStateBuilder {
        AppStateBuilder {
            runtime,
            read,
            events,
            auth,
            router: None,
            evaluator: None,
            memory: None,
            workers: None,
            artifacts: None,
            config: None,
        }
    }

    /// The write side.
    #[must_use]
    pub fn runtime(&self) -> &Arc<dyn RuntimePort> {
        &self.inner.runtime
    }

    /// The read models.
    #[must_use]
    pub fn read(&self) -> &Arc<dyn ReadPort> {
        &self.inner.read
    }

    /// History + live bus behind the SSE endpoints.
    #[must_use]
    pub fn events(&self) -> &Arc<dyn EventsPort> {
        &self.inner.events
    }

    /// The routing leaderboard, when a router is wired up.
    pub fn router(&self) -> Result<&Arc<dyn RouterPort>, ApiError> {
        self.inner
            .router
            .as_ref()
            .ok_or_else(|| ApiError::runtime_unavailable("the router"))
    }

    /// The proposals inbox, when an evaluator is wired up.
    pub fn evaluator(&self) -> Result<&Arc<dyn EvaluatorPort>, ApiError> {
        self.inner
            .evaluator
            .as_ref()
            .ok_or_else(|| ApiError::runtime_unavailable("the evaluator"))
    }

    /// The memory store, when one is wired up.
    pub fn memory(&self) -> Result<&Arc<dyn MemoryPort>, ApiError> {
        self.inner
            .memory
            .as_ref()
            .ok_or_else(|| ApiError::runtime_unavailable("the memory store"))
    }

    /// The worker registry, when one is wired up.
    pub fn workers(&self) -> Result<&Arc<dyn WorkersPort>, ApiError> {
        self.inner
            .workers
            .as_ref()
            .ok_or_else(|| ApiError::runtime_unavailable("the worker registry"))
    }

    /// Artifact bytes.
    #[must_use]
    pub fn artifacts(&self) -> &Arc<dyn ArtifactsPort> {
        &self.inner.artifacts
    }

    /// The effective configuration with its provenance.
    #[must_use]
    pub fn config(&self) -> &Arc<Resolved> {
        &self.inner.config
    }

    /// The `[server]` section.
    #[must_use]
    pub fn server(&self) -> &Server {
        &self.inner.server
    }

    /// The bearer token verifier (SIGHUP calls `reload` on it).
    #[must_use]
    pub fn auth(&self) -> &Arc<TokenVerifier> {
        &self.inner.auth
    }

    /// The process-local `Idempotency-Key` cache.
    #[must_use]
    pub fn idempotency(&self) -> &Idempotency {
        &self.inner.idempotency
    }

    /// The per-token rate limiter.
    #[must_use]
    pub fn rate_limiter(&self) -> &RateLimiter {
        &self.inner.rate
    }

    /// The SSE connection cap.
    #[must_use]
    pub fn sse_gate(&self) -> &SseGate {
        &self.inner.sse
    }
}

/// Builder for [`AppState`]; optional ports degrade to `503 runtime_unavailable`.
#[derive(Debug)]
pub struct AppStateBuilder {
    runtime: Arc<dyn RuntimePort>,
    read: Arc<dyn ReadPort>,
    events: Arc<dyn EventsPort>,
    auth: Arc<TokenVerifier>,
    router: Option<Arc<dyn RouterPort>>,
    evaluator: Option<Arc<dyn EvaluatorPort>>,
    memory: Option<Arc<dyn MemoryPort>>,
    workers: Option<Arc<dyn WorkersPort>>,
    artifacts: Option<Arc<dyn ArtifactsPort>>,
    config: Option<Arc<Resolved>>,
}

impl AppStateBuilder {
    /// Wires up `GET /api/v1/routes`.
    #[must_use]
    pub fn router_port(mut self, port: Arc<dyn RouterPort>) -> Self {
        self.router = Some(port);
        self
    }

    /// Wires up the proposals endpoints.
    #[must_use]
    pub fn evaluator(mut self, port: Arc<dyn EvaluatorPort>) -> Self {
        self.evaluator = Some(port);
        self
    }

    /// Wires up the memory endpoints.
    #[must_use]
    pub fn memory(mut self, port: Arc<dyn MemoryPort>) -> Self {
        self.memory = Some(port);
        self
    }

    /// Wires up `GET /api/v1/workers`.
    #[must_use]
    pub fn workers(mut self, port: Arc<dyn WorkersPort>) -> Self {
        self.workers = Some(port);
        self
    }

    /// Overrides the artifact byte source (default: `file://` from disk).
    #[must_use]
    pub fn artifacts(mut self, port: Arc<dyn ArtifactsPort>) -> Self {
        self.artifacts = Some(port);
        self
    }

    /// The effective configuration (`GET /api/v1/config`, `[server]` limits).
    #[must_use]
    pub fn config(mut self, config: Arc<Resolved>) -> Self {
        self.config = Some(config);
        self
    }

    /// Freezes the state.
    #[must_use]
    pub fn build(self) -> AppState {
        let config = self.config.unwrap_or_else(|| {
            Arc::new(Resolved {
                config: kevin_config::KevinConfig::default(),
                sources: kevin_config::Sources::new(),
            })
        });
        let server = config.config.server.clone();
        AppState {
            inner: Arc::new(Inner {
                runtime: self.runtime,
                read: self.read,
                events: self.events,
                router: self.router,
                evaluator: self.evaluator,
                memory: self.memory,
                workers: self.workers,
                artifacts: self
                    .artifacts
                    .unwrap_or_else(|| Arc::new(crate::adapters::FileArtifacts)),
                config,
                server,
                auth: self.auth,
                idempotency: Idempotency::default(),
                rate: RateLimiter::default(),
                sse: SseGate::default(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

/// Entries older than this are evicted (a client that replays later simply
/// gets the durable behaviour of `core.processed_commands`).
const IDEMPOTENCY_TTL: Duration = Duration::from_hours(24);
/// Cap on remembered keys, so a hostile client cannot grow the map forever.
const IDEMPOTENCY_CAPACITY: usize = 4096;

/// What a replayed `Idempotency-Key` returns.
#[derive(Debug, Clone)]
pub enum Replay {
    /// The key is new; the caller must perform the command and then call
    /// [`Idempotency::remember`].
    Fresh,
    /// The key was used with the *same* body: return this response verbatim.
    Same(serde_json::Value),
    /// The key was used with a different body → `409 idempotency_conflict`.
    Conflict,
}

#[derive(Debug, Default)]
struct Recorded {
    body_hash: [u8; 32],
    response: serde_json::Value,
    at: Option<Instant>,
}

/// Process-local `Idempotency-Key` cache (plan/07 §Conventions).
///
/// The **durable** guarantee is the command log: the key becomes the
/// `command_id`, and `core.processed_commands` makes the write itself
/// exactly-once across restarts. This cache adds the HTTP-level half of the
/// contract — replay with an identical body returns the original response,
/// replay with a different body is a `409` — which needs the request body and
/// therefore cannot live in the store.
#[derive(Debug, Default)]
pub struct Idempotency {
    entries: Mutex<HashMap<String, Recorded>>,
}

impl Idempotency {
    /// Longest accepted key (plan/07 §Conventions).
    pub const MAX_KEY_LEN: usize = 128;

    /// Validates the header value: `[A-Za-z0-9._:-]{1,128}`.
    pub fn validate(key: &str) -> Result<(), ApiError> {
        if key.is_empty() || key.len() > Self::MAX_KEY_LEN {
            return Err(ApiError::invalid_request(
                "Idempotency-Key must be 1..=128 characters",
            ));
        }
        if !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
        {
            return Err(ApiError::invalid_request(
                "Idempotency-Key may only contain [A-Za-z0-9._:-]",
            ));
        }
        Ok(())
    }

    /// The `command_id` an idempotency key maps to: a uuid v5 of the key in a
    /// fixed namespace, so the same key always yields the same command id (and
    /// therefore the same `core.processed_commands` row) on every process.
    #[must_use]
    pub fn command_id(key: &str) -> kevin_domain::ids::CommandId {
        // A stable, arbitrary namespace uuid for Kevin idempotency keys.
        const NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
            0x6b, 0x65, 0x76, 0x69, 0x6e, 0x2d, 0x61, 0x70, 0x69, 0x2d, 0x69, 0x64, 0x65, 0x6d,
            0x70, 0x6f,
        ]);
        kevin_domain::ids::CommandId::from_uuid(uuid::Uuid::new_v5(&NAMESPACE, key.as_bytes()))
    }

    /// Looks `key` up for a request whose body hashes to `body_hash`.
    #[must_use]
    pub fn check(&self, key: &str, body_hash: [u8; 32]) -> Replay {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict(&mut entries);
        match entries.get(key) {
            None => Replay::Fresh,
            Some(recorded) if recorded.body_hash == body_hash => {
                Replay::Same(recorded.response.clone())
            }
            Some(_) => Replay::Conflict,
        }
    }

    /// Records the response `key` produced.
    pub fn remember(&self, key: &str, body_hash: [u8; 32], response: serde_json::Value) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::evict(&mut entries);
        if entries.len() >= IDEMPOTENCY_CAPACITY {
            entries.clear();
        }
        entries.insert(
            key.to_owned(),
            Recorded {
                body_hash,
                response,
                at: Some(Instant::now()),
            },
        );
    }

    fn evict(entries: &mut HashMap<String, Recorded>) {
        entries.retain(|_, e| e.at.is_none_or(|at| at.elapsed() < IDEMPOTENCY_TTL));
    }
}

/// SHA-256 of a request body, the identity an `Idempotency-Key` is bound to.
#[must_use]
pub fn body_hash(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Per-token bucket: `RATE_LIMIT_PER_SECOND` sustained, `RATE_LIMIT_BURST` burst.
#[derive(Debug, Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Consumes one request for `key`; `false` means `429 rate_limited`.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() > 1024 {
            buckets.retain(|_, b| now.duration_since(b.last) < Duration::from_secs(60));
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: RATE_LIMIT_BURST,
            last: now,
        });
        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        bucket.tokens = (bucket.tokens + elapsed * RATE_LIMIT_PER_SECOND).min(RATE_LIMIT_BURST);
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

// ---------------------------------------------------------------------------
// SSE connection cap
// ---------------------------------------------------------------------------

/// Counts open SSE streams so one client cannot exhaust the server's
/// connections (plan/07 §Limits: ≤ 64 per token).
#[derive(Debug, Default)]
pub struct SseGate {
    open: Arc<AtomicU32>,
    per_key: RwLock<HashMap<String, Arc<AtomicU32>>>,
}

/// Holds one SSE slot; releases it (and updates `kevin_api_sse_connections`)
/// when the stream is dropped.
#[derive(Debug)]
pub struct SsePermit {
    total: Arc<AtomicU32>,
    key: Arc<AtomicU32>,
}

impl Drop for SsePermit {
    fn drop(&mut self) {
        let total = self.total.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        self.key.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!(API_SSE_CONNECTIONS).set(f64::from(total));
    }
}

impl SseGate {
    /// Reserves a slot for `key`, or `429 rate_limited` when the cap is reached.
    pub fn acquire(&self, key: &str) -> Result<SsePermit, ApiError> {
        let counter = {
            let map = self
                .per_key
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.get(key).map(Arc::clone)
        };
        let counter = counter.unwrap_or_else(|| {
            let mut map = self
                .per_key
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(map.entry(key.to_owned()).or_default())
        });
        if counter.load(Ordering::Relaxed) >= MAX_SSE_CONNECTIONS {
            return Err(ApiError::new(
                ErrorCode::RateLimited,
                format!("at most {MAX_SSE_CONNECTIONS} concurrent SSE streams per token"),
            ));
        }
        counter.fetch_add(1, Ordering::Relaxed);
        let total = self.open.fetch_add(1, Ordering::Relaxed) + 1;
        metrics::gauge!(API_SSE_CONNECTIONS).set(f64::from(total));
        Ok(SsePermit {
            total: Arc::clone(&self.open),
            key: counter,
        })
    }

    /// Streams currently open.
    #[must_use]
    pub fn open(&self) -> u32 {
        self.open.load(Ordering::Relaxed)
    }
}

/// `kevin_api_sse_connections` (plan/10 §Metrics).
const API_SSE_CONNECTIONS: &str = "kevin_api_sse_connections";

// ---------------------------------------------------------------------------
// Loopback guard
// ---------------------------------------------------------------------------

/// Whether `peer` is a loopback address. Requests without connection info
/// (in-process `oneshot` calls, unix sockets) count as loopback.
#[must_use]
pub fn is_loopback(peer: Option<SocketAddr>) -> bool {
    match peer.map(|addr| addr.ip()) {
        None => true,
        Some(IpAddr::V4(ip)) => ip.is_loopback(),
        Some(IpAddr::V6(ip)) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Idempotency, RateLimiter, Replay, body_hash, is_loopback};

    #[test]
    fn idempotency_keys_are_validated() {
        assert!(Idempotency::validate("cli-0191f3a0-abc").is_ok());
        assert!(Idempotency::validate("").is_err());
        assert!(Idempotency::validate("has space").is_err());
        assert!(Idempotency::validate(&"x".repeat(129)).is_err());
    }

    #[test]
    fn the_same_key_maps_to_the_same_command_id() {
        assert_eq!(
            Idempotency::command_id("cli-1"),
            Idempotency::command_id("cli-1")
        );
        assert_ne!(
            Idempotency::command_id("cli-1"),
            Idempotency::command_id("cli-2")
        );
    }

    #[test]
    fn replay_distinguishes_same_body_from_conflict() {
        let cache = Idempotency::default();
        let a = body_hash(b"{\"goal\":\"a\"}");
        let b = body_hash(b"{\"goal\":\"b\"}");
        assert!(matches!(cache.check("k", a), Replay::Fresh));
        cache.remember("k", a, serde_json::json!({"id": "1"}));
        assert!(matches!(cache.check("k", a), Replay::Same(_)));
        assert!(matches!(cache.check("k", b), Replay::Conflict));
    }

    #[test]
    fn the_bucket_allows_the_burst_and_then_refuses() {
        const BURST: u32 = 120;
        assert!((super::RATE_LIMIT_BURST - f64::from(BURST)).abs() < f64::EPSILON);
        let limiter = RateLimiter::default();
        for _ in 0..BURST {
            assert!(limiter.allow("t"));
        }
        assert!(!limiter.allow("t"), "the burst is exhausted");
        assert!(limiter.allow("other"), "buckets are per token");
    }

    #[test]
    fn missing_connection_info_counts_as_loopback() {
        assert!(is_loopback(None));
        assert!(is_loopback(Some("127.0.0.1:1".parse().expect("addr"))));
        assert!(!is_loopback(Some("10.0.0.1:1".parse().expect("addr"))));
    }
}
