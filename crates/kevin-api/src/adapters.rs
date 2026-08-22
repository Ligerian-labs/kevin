//! Concrete implementations of the **read-side** ports (`server` feature only).
//!
//! The write side lives in [`crate::runtime`]: it needs the orchestrator's
//! command services, while everything here needs only a pool, a bus or a
//! registry, so keeping them apart lets a read-only deployment (a mirror, a
//! dashboard) leave the engine out entirely.

use std::sync::Arc;

use async_trait::async_trait;
use kevin_bus::{BusStream, EventBus, SubscriptionFilter};
use kevin_domain::Actor;
use kevin_domain::ids::{ArtifactId, MemoryItemId, QuestionId, RunId, TaskId};
use kevin_domain::values::MemoryKind;
use kevin_memory::store::{ForgetFilter, MemoryStore, SearchQuery};
use kevin_orchestrator::projections::{
    CostGroupBy, CostQuery, QuestionQuery, ReadModels, RunQuery, TaskLogQuery,
};
use kevin_store::EventStore;
use kevin_worker::registry::WorkerRegistry;

use crate::convert;
use crate::dto::{
    ArtifactDto, CostQueryDto, CostReportDto, EventDto, LessonsQuery, ListRunsQuery, MemoryItemDto,
    MemorySearchQuery, Page, QuestionDto, QuestionsQuery, RouteScoreDto, RunDto, RunSummaryDto,
    TaskDto, TaskLogLineDto, TaskLogQueryDto, WorkerDoctorDto,
};
use crate::port::{
    ArtifactsPort, EventsPort, MemoryPort, PortResult, ReadPort, RouterPort, RuntimeError,
    WorkersPort,
};

/// How many events one catch-up batch reads from the store.
pub const CATCH_UP_BATCH: usize = 256;

// ---------------------------------------------------------------------------
// Read models
// ---------------------------------------------------------------------------

/// [`ReadPort`] over `kevin_orchestrator::projections::ReadModels`.
#[derive(Debug, Clone)]
pub struct ProjectionReads {
    read: ReadModels,
}

impl ProjectionReads {
    /// Wraps the typed projection queries.
    #[must_use]
    pub const fn new(read: ReadModels) -> Self {
        Self { read }
    }
}

impl From<ReadModels> for ProjectionReads {
    fn from(read: ReadModels) -> Self {
        Self::new(read)
    }
}

#[async_trait]
impl ReadPort for ProjectionReads {
    async fn run(&self, run_id: RunId) -> PortResult<Option<RunDto>> {
        let Some(row) = self.read.run(run_id.as_uuid()).await? else {
            return Ok(None);
        };
        let tasks = self.read.tasks_of_run(run_id.as_uuid()).await?;
        Ok(Some(convert::run(&row, &tasks)))
    }

    async fn runs(&self, query: &ListRunsQuery) -> PortResult<Page<RunSummaryDto>> {
        let page = self
            .read
            .runs(&RunQuery {
                status: query.status.clone(),
                cursor: query.cursor.clone(),
                limit: query.limit,
            })
            .await?;
        Ok(Page {
            items: page.items.iter().map(convert::run_summary).collect(),
            next_cursor: page.next_cursor,
        })
    }

    async fn tasks_of_run(&self, run_id: RunId) -> PortResult<Vec<TaskDto>> {
        let rows = self.read.tasks_of_run(run_id.as_uuid()).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let artifacts = self.read.artifacts_of_task(row.task_id).await?;
            out.push(convert::task(
                row,
                artifacts.iter().map(convert::artifact).collect(),
            ));
        }
        Ok(out)
    }

    async fn task(&self, task_id: TaskId) -> PortResult<Option<TaskDto>> {
        let Some(row) = self.read.task(task_id.as_uuid()).await? else {
            return Ok(None);
        };
        let artifacts = self.read.artifacts_of_task(task_id.as_uuid()).await?;
        Ok(Some(convert::task(
            &row,
            artifacts.iter().map(convert::artifact).collect(),
        )))
    }

    async fn task_log(
        &self,
        task_id: TaskId,
        query: &TaskLogQueryDto,
    ) -> PortResult<Page<TaskLogLineDto>> {
        let page = self
            .read
            .task_log(&TaskLogQuery {
                task_id: task_id.as_uuid(),
                attempt: query.attempt.map(i32::from),
                after_seq: query.after_seq,
                limit: query.limit,
            })
            .await?;
        Ok(Page {
            items: page.items.iter().map(convert::task_log_line).collect(),
            next_cursor: page.next_cursor,
        })
    }

    async fn artifacts_of_task(&self, task_id: TaskId) -> PortResult<Vec<ArtifactDto>> {
        let rows = self.read.artifacts_of_task(task_id.as_uuid()).await?;
        Ok(rows.iter().map(convert::artifact).collect())
    }

    async fn artifact(&self, artifact_id: ArtifactId) -> PortResult<Option<ArtifactDto>> {
        Ok(self
            .read
            .artifact(artifact_id.as_uuid())
            .await?
            .as_ref()
            .map(convert::artifact))
    }

    async fn question(&self, question_id: QuestionId) -> PortResult<Option<QuestionDto>> {
        Ok(self
            .read
            .question(question_id.as_uuid())
            .await?
            .as_ref()
            .map(convert::question))
    }

    async fn questions(&self, query: &QuestionsQuery) -> PortResult<Page<QuestionDto>> {
        let page = self
            .read
            .questions(&QuestionQuery {
                run_id: query.run_id.map(|id| id.as_uuid()),
                status: query.status.clone(),
                cursor: query.cursor.clone(),
                limit: query.limit,
            })
            .await?;
        Ok(Page {
            items: page.items.iter().map(convert::question).collect(),
            next_cursor: page.next_cursor,
        })
    }

    async fn cost(&self, query: &CostQueryDto) -> PortResult<CostReportDto> {
        let group_by = match query.group_by.as_deref() {
            Some("model") => CostGroupBy::Model,
            Some("kind") => CostGroupBy::Kind,
            _ => CostGroupBy::Run,
        };
        let report = self
            .read
            .cost(&CostQuery {
                since: query.since,
                run_id: query.run_id.map(|id| id.as_uuid()),
                group_by,
            })
            .await?;
        Ok(convert::cost_report(&report))
    }
}

impl From<kevin_orchestrator::projections::ProjectionError> for RuntimeError {
    fn from(err: kevin_orchestrator::projections::ProjectionError) -> Self {
        use kevin_orchestrator::projections::ProjectionError;
        match err {
            ProjectionError::InvalidCursor { cursor } => {
                RuntimeError::Internal(format!("invalid cursor `{cursor}`"))
            }
            other => RuntimeError::Storage(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// [`EventsPort`] over the event store (catch-up) and the bus (live).
#[derive(Clone)]
pub struct StoreEvents {
    store: Arc<dyn EventStore>,
    bus: Arc<dyn EventBus>,
}

impl std::fmt::Debug for StoreEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreEvents")
            .field("head", &self.bus.position())
            .finish_non_exhaustive()
    }
}

impl StoreEvents {
    /// Wraps a store and a bus.
    #[must_use]
    pub const fn new(store: Arc<dyn EventStore>, bus: Arc<dyn EventBus>) -> Self {
        Self { store, bus }
    }
}

#[async_trait]
impl EventsPort for StoreEvents {
    async fn after(&self, from: u64, limit: usize) -> PortResult<Vec<EventDto>> {
        let events = self
            .store
            .read_all(from, limit)
            .await
            .map_err(|e| RuntimeError::Storage(e.to_string()))?;
        Ok(events.iter().map(convert::event).collect())
    }

    fn subscribe_live(&self) -> BusStream {
        self.bus
            .subscribe(SubscriptionFilter::all().named("api-sse"))
    }

    fn head(&self) -> u64 {
        self.bus.position()
    }
}

// ---------------------------------------------------------------------------
// Workers
// ---------------------------------------------------------------------------

/// [`WorkersPort`] over `WorkerRegistry::doctor_all`.
#[derive(Debug, Clone)]
pub struct RegistryWorkers {
    registry: Arc<WorkerRegistry>,
}

impl RegistryWorkers {
    /// Wraps a registry.
    #[must_use]
    pub const fn new(registry: Arc<WorkerRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl WorkersPort for RegistryWorkers {
    async fn doctor(&self) -> PortResult<Vec<WorkerDoctorDto>> {
        let reports = self.registry.doctor_all().await;
        Ok(reports
            .iter()
            .map(|doctor| convert::worker_doctor(doctor, true))
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// [`MemoryPort`] over `kevin_memory::MemoryStore`.
#[derive(Clone)]
pub struct StoreMemory {
    store: Arc<MemoryStore>,
}

impl std::fmt::Debug for StoreMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreMemory").finish_non_exhaustive()
    }
}

impl StoreMemory {
    /// Wraps a memory store.
    #[must_use]
    pub const fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

/// `"lesson,fact"` → the kinds; unknown names are ignored.
fn parse_kinds(list: Option<&str>) -> Vec<MemoryKind> {
    list.into_iter()
        .flat_map(|list| list.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| MemoryKind::ALL.into_iter().find(|k| k.as_str() == name))
        .collect()
}

#[async_trait]
impl MemoryPort for StoreMemory {
    async fn search(&self, query: &MemorySearchQuery) -> PortResult<Vec<MemoryItemDto>> {
        let mut search = SearchQuery::new(query.q.clone());
        search.kinds = parse_kinds(query.kinds.as_deref());
        if let Some(top_k) = query.top_k {
            search.top_k = top_k;
        }
        let hits = self
            .store
            .search(search)
            .await
            .map_err(|e| RuntimeError::Storage(e.to_string()))?;
        Ok(hits.iter().map(convert::memory_hit).collect())
    }

    async fn lessons(&self, query: &LessonsQuery) -> PortResult<Page<MemoryItemDto>> {
        let limit = query.limit.unwrap_or(50).clamp(1, 200);
        let lessons = self
            .store
            .lessons(&kevin_memory::item::ScopeFilter::Global, limit)
            .await
            .map_err(|e| RuntimeError::Storage(e.to_string()))?;
        Ok(Page::new(lessons.iter().map(convert::lesson).collect()))
    }

    async fn forget(&self, item_id: MemoryItemId, actor: Actor) -> PortResult<()> {
        self.store
            .forget(item_id, actor)
            .await
            .map_err(|e| match e {
                kevin_memory::MemoryError::NotFound(_) => {
                    RuntimeError::Internal(format!("memory item {item_id} does not exist"))
                }
                other => RuntimeError::Storage(other.to_string()),
            })?;
        let _ = ForgetFilter::Id(item_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Artifacts
// ---------------------------------------------------------------------------

/// [`ArtifactsPort`] that serves `file://` artifacts from disk and returns
/// remote references (`pr_url`, `https://…`) as `text/uri-list`.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileArtifacts;

/// Content type for an artifact kind (plan/07 §Endpoints).
#[must_use]
pub fn content_type_of(kind: &str) -> &'static str {
    match kind {
        "diff" => "text/x-diff; charset=utf-8",
        "json" => "application/json",
        "pr_url" => "text/uri-list; charset=utf-8",
        "report" | "transcript" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[async_trait]
impl ArtifactsPort for FileArtifacts {
    async fn read(&self, artifact: &ArtifactDto) -> PortResult<(String, Vec<u8>)> {
        let content_type = content_type_of(&artifact.kind).to_owned();
        let Some(path) = artifact.uri.strip_prefix("file://") else {
            // Nothing to stream: hand the reference back as its own body.
            return Ok((
                "text/uri-list; charset=utf-8".to_owned(),
                artifact.uri.clone().into_bytes(),
            ));
        };
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok((content_type, bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(RuntimeError::ArtifactNotFound(artifact.id))
            }
            Err(err) => Err(RuntimeError::internal(err)),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing leaderboard
// ---------------------------------------------------------------------------

/// [`RouterPort`] over `kevin_router`'s score repository (`routing.route_scores`).
#[derive(Debug, Clone)]
pub struct RepoRoutes {
    repo: Arc<dyn kevin_router::score::RouteScoreRepo>,
}

impl RepoRoutes {
    /// Wraps a route-score repository.
    #[must_use]
    pub const fn new(repo: Arc<dyn kevin_router::score::RouteScoreRepo>) -> Self {
        Self { repo }
    }
}

#[async_trait]
impl RouterPort for RepoRoutes {
    async fn leaderboard(&self, kind: Option<&str>) -> PortResult<Vec<RouteScoreDto>> {
        let kind = kind
            .map(str::parse::<kevin_domain::TaskKind>)
            .transpose()
            .map_err(|e| RuntimeError::Internal(format!("unknown task kind: {e}")))?;
        let rows = self
            .repo
            .leaderboard(kind.as_ref())
            .await
            .map_err(|e| RuntimeError::Storage(e.to_string()))?;
        Ok(rows.iter().map(convert::route_score).collect())
    }
}
