//! The pgvector-backed memory store (`plan/06-memory-and-learning.md` §1.3–1.5,
//! §1.8).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use kevin_domain::{Actor, MemoryItemEvent, MemoryItemId, RunId};
use kevin_store::{EventStore, PgPool};
use kevin_telemetry::Redactor;
use kevin_telemetry::metrics::{
    EMBEDDING_DURATION_SECONDS, MEMORY_ITEMS, MEMORY_SEARCH_DURATION_SECONDS,
};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Row as _};
use uuid::Uuid;

use crate::config::MemoryCfg;
use crate::embed::{EmbedError, Embedder, NOOP_MODEL, embed_one, truncate_input};
use crate::error::{MemoryError, Result};
use crate::events;
use crate::item::{
    MAX_CONTENT_CHARS, MemoryKind, MemoryRecord, MemoryScope, MemorySource, RepoId, ScopeFilter,
    parse_kind, scope_label,
};
use crate::rank;

/// Gauge of items stored without a vector, waiting for `kevin memory reindex`.
///
/// Declared here (not in `kevin_telemetry::metrics`, owned by WS-04) so this
/// workstream adds no metric to the shared registry; move it there when the
/// name is adopted.
// TODO(ws-04): promote to `kevin_telemetry::metrics` if the registry accepts it.
pub const MEMORY_REINDEX_PENDING: &str = "kevin_memory_reindex_pending";

/// Columns of `memory.memory_items` every read shares (never the vector).
const ITEM_COLUMNS: &str = "id, kind, content, tags, source, scope, importance, embedding_model, \
     created_at, superseded_by, forgotten_at";

/// The same columns qualified with the `i` alias (the search query joins two
/// candidate CTEs, so bare names would be ambiguous).
const ITEM_COLUMNS_QUALIFIED: &str = "i.id, i.kind, i.content, i.tags, i.source, i.scope, \
     i.importance, i.embedding_model, i.created_at, i.superseded_by, i.forgotten_at";

/// Predicate shared by both candidate legs of the hybrid search.
const SEARCH_FILTERS: &str = "forgotten_at IS NULL AND superseded_by IS NULL \
     AND ($3::text[] IS NULL OR kind = ANY($3)) \
     AND ($4::text[] IS NULL OR tags && $4) \
     AND scope = ANY($5)";

/// Command handled by [`MemoryStore::store`] (`StoreMemoryItem`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoreRequest {
    /// Kind of item.
    pub kind: MemoryKind,
    /// Content (≤ 8 000 characters, secret-free).
    pub content: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Scope.
    pub scope: MemoryScope,
    /// Provenance.
    pub source: MemorySource,
    /// Importance in `0..=1` (defaults come from the kind).
    pub importance: f32,
}

impl StoreRequest {
    /// A request of `kind` with the kind's default importance, global scope
    /// and a system actor.
    pub fn new(kind: MemoryKind, content: impl Into<String>) -> Self {
        Self {
            kind,
            content: content.into(),
            tags: Vec::new(),
            scope: MemoryScope::Global,
            source: MemorySource::from_actor(Actor::system("memory")),
            importance: kind.default_importance(),
        }
    }

    /// A `fact` (what `kevin memory add --kind fact` builds).
    pub fn fact(content: impl Into<String>) -> Self {
        Self::new(MemoryKind::Fact, content)
    }

    /// A `lesson` (what the evaluator's auto-apply builds).
    pub fn lesson(content: impl Into<String>) -> Self {
        Self::new(MemoryKind::Lesson, content)
    }

    /// Sets the tags.
    #[must_use]
    pub fn with_tags(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the scope.
    #[must_use]
    pub fn with_scope(mut self, scope: MemoryScope) -> Self {
        self.scope = scope;
        self
    }

    /// Sets the provenance.
    #[must_use]
    pub fn with_source(mut self, source: MemorySource) -> Self {
        self.source = source;
        self
    }

    /// Sets the importance (clamped to `0..=1`).
    #[must_use]
    pub fn with_importance(mut self, importance: f32) -> Self {
        self.importance = importance.clamp(0.0, 1.0);
        self
    }
}

/// A hybrid search (`plan/06` §1.4).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchQuery {
    /// Query text (embedded once, also used for `websearch_to_tsquery`).
    pub text: String,
    /// Restrict to these kinds (empty = every kind).
    pub kinds: Vec<MemoryKind>,
    /// Keep items carrying at least one of these tags (empty = no tag filter).
    pub tags_any: Vec<String>,
    /// Scope restriction.
    pub scope: ScopeFilter,
    /// Hits to return; `0` means `memory.top_k`.
    pub top_k: usize,
    /// Cosine floor; negative means `memory.min_similarity`.
    pub min_similarity: f32,
}

impl SearchQuery {
    /// A global query using the configured `top_k`/`min_similarity`.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kinds: Vec::new(),
            tags_any: Vec::new(),
            scope: ScopeFilter::Global,
            top_k: 0,
            min_similarity: -1.0,
        }
    }

    /// Restricts to these kinds.
    #[must_use]
    pub fn with_kinds(mut self, kinds: impl IntoIterator<Item = MemoryKind>) -> Self {
        self.kinds = kinds.into_iter().collect();
        self
    }

    /// Restricts to items carrying one of these tags.
    #[must_use]
    pub fn with_tags_any(mut self, tags: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.tags_any = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the scope restriction.
    #[must_use]
    pub fn with_scope(mut self, scope: ScopeFilter) -> Self {
        self.scope = scope;
        self
    }

    /// Sets the number of hits.
    #[must_use]
    pub const fn with_top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }

    /// Sets the cosine floor.
    #[must_use]
    pub const fn with_min_similarity(mut self, min_similarity: f32) -> Self {
        self.min_similarity = min_similarity;
        self
    }
}

/// One ranked search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The item (row form; the aggregate is `kevin_domain::MemoryItem`).
    pub item: MemoryRecord,
    /// `1 - cosine_distance` (0 when the item has no vector).
    pub similarity: f32,
    /// `ts_rank_cd` normalised over the candidate set.
    pub lexical: f32,
    /// The hybrid score this hit was sorted by.
    pub score: f32,
}

/// What `kevin memory forget` may target (`plan/09-security.md` §Memory privacy).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgetFilter {
    /// One item.
    Id(MemoryItemId),
    /// Everything learned during a run.
    Run(RunId),
    /// Everything scoped to a repository.
    Repo(RepoId),
    /// Everything created before an instant.
    Before(DateTime<Utc>),
}

impl ForgetFilter {
    /// `--all-before <RFC 3339 date>` (parsed here so callers need no chrono).
    pub fn before_rfc3339(date: &str) -> Result<Self> {
        let parsed = DateTime::parse_from_rfc3339(date.trim()).map_err(|e| {
            MemoryError::Invalid(format!("`{date}` is not an RFC 3339 date/time: {e}"))
        })?;
        Ok(ForgetFilter::Before(parsed.with_timezone(&Utc)))
    }
}

/// What `reindex` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReindexReport {
    /// Items re-embedded by this call.
    pub embedded: usize,
    /// Items that still needed work when the call started.
    pub total: usize,
    /// Model the items now carry.
    pub model: String,
}

/// Counts behind `kevin memory doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemoryCounts {
    /// Live items per kind.
    pub by_kind: BTreeMap<String, i64>,
    /// Live items (not forgotten, not superseded).
    pub live: i64,
    /// Forgotten items (kept for provenance).
    pub forgotten: i64,
    /// Live items without a vector (waiting for `reindex`).
    pub pending_embedding: i64,
    /// Distinct embedding models present.
    pub models: Vec<String>,
}

/// One row of `memory.lessons_view`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    /// Item id.
    pub id: MemoryItemId,
    /// Lesson text.
    pub content: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Scope.
    pub scope: MemoryScope,
    /// Importance.
    pub importance: f32,
    /// Run the lesson came from, when known.
    pub run_id: Option<String>,
    /// When it was learned.
    pub created_at: DateTime<Utc>,
}

/// One item of `kevin memory export --json` (embeddings excluded by default).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportItem {
    /// The item.
    #[serde(flatten)]
    pub item: MemoryRecord,
    /// The stored vector, only when exported with embeddings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

/// What `import` did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportReport {
    /// Items inserted.
    pub imported: usize,
    /// Items already present (same id).
    pub skipped: usize,
    /// Items refused (secret content, too long, …) with the reason.
    pub refused: Vec<(MemoryItemId, String)>,
}

/// Persistent retrieval memory over `memory.memory_items`.
#[derive(Clone)]
pub struct MemoryStore {
    pool: PgPool,
    embedder: Arc<dyn Embedder>,
    cfg: MemoryCfg,
    redactor: Redactor,
    events: Option<Arc<dyn EventStore>>,
}

impl fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStore")
            .field("embedder", &self.embedder.model_name())
            .field("dimensions", &self.embedder.dimensions())
            .field("cfg", &self.cfg)
            .field("events", &self.events.is_some())
            .finish_non_exhaustive()
    }
}

impl MemoryStore {
    /// A store over `pool` using `embedder` and the resolved `[memory]` config.
    #[must_use]
    pub fn new(pool: PgPool, embedder: Arc<dyn Embedder>, cfg: MemoryCfg) -> Self {
        Self {
            pool,
            embedder,
            cfg,
            redactor: Redactor::global().clone(),
            events: None,
        }
    }

    /// Also append `memory.*` events through the event store.
    #[must_use]
    pub fn with_events(mut self, events: Arc<dyn EventStore>) -> Self {
        self.events = Some(events);
        self
    }

    /// Use a specific redactor (default: the process-wide one).
    #[must_use]
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// The pool this store reads and writes.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// The resolved configuration.
    #[must_use]
    pub fn cfg(&self) -> &MemoryCfg {
        &self.cfg
    }

    /// The embedder in use.
    #[must_use]
    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    // -- writes ------------------------------------------------------------

    /// Stores one item: redaction gate, embedding (or `NULL` when the embedder
    /// is disabled/failing), insert, `memory.item_stored`.
    pub async fn store(&self, req: StoreRequest) -> Result<MemoryItemId> {
        self.ensure_enabled()?;
        let content = self.vet_content(&req.content)?;
        let embedding = self.embed_for_storage(&content).await?;
        let id = MemoryItemId::new();
        self.insert(id, &req, &content, embedding.as_ref()).await?;
        metrics::gauge!(MEMORY_ITEMS, "kind" => req.kind.as_str(), "scope" => scope_label(&req.scope))
            .increment(1.0);
        self.emit(
            id,
            &MemoryItemEvent::Stored {
                memory_item_id: id,
                kind: req.kind,
                content: content.clone(),
                tags: req.tags.clone(),
                source: req.source.clone(),
                scope: req.scope.clone(),
                embedding_model: embedding
                    .as_ref()
                    .map(|_| self.embedder.model_name().to_owned()),
                importance: req.importance,
                created_at: Utc::now(),
            },
            req.source.actor.clone(),
        )
        .await?;
        tracing::debug!(item = %id, kind = %req.kind, scope = %req.scope, "memory item stored");
        Ok(id)
    }

    /// Stores `req` and links `old` to it (`memory.item_superseded`).
    pub async fn supersede(&self, old: MemoryItemId, req: StoreRequest) -> Result<MemoryItemId> {
        self.ensure_enabled()?;
        let actor = req.source.actor.clone();
        let new_id = self.store(req).await?;
        let updated = sqlx::query(
            "UPDATE memory.memory_items SET superseded_by = $2 WHERE id = $1 AND superseded_by IS NULL",
        )
        .bind(old.as_uuid())
        .bind(new_id.as_uuid())
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(MemoryError::NotFound(old));
        }
        self.emit(
            old,
            &MemoryItemEvent::Superseded {
                superseded_by: new_id,
            },
            actor,
        )
        .await?;
        Ok(new_id)
    }

    /// Forgets one item: `forgotten_at` set, content blanked, embedding
    /// dropped; the row stays for provenance (`memory.item_forgotten`).
    pub async fn forget(&self, id: MemoryItemId, by: Actor) -> Result<()> {
        let exists: Option<Option<DateTime<Utc>>> =
            sqlx::query_scalar("SELECT forgotten_at FROM memory.memory_items WHERE id = $1")
                .bind(id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;
        match exists {
            None => return Err(MemoryError::NotFound(id)),
            Some(Some(_)) => return Ok(()), // already forgotten: idempotent
            Some(None) => {}
        }
        sqlx::query(
            "UPDATE memory.memory_items \
             SET forgotten_at = now(), content = '', embedding = NULL, embedding_model = NULL \
             WHERE id = $1 AND forgotten_at IS NULL",
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await?;
        self.emit(
            id,
            &MemoryItemEvent::Forgotten {
                reason: String::new(),
            },
            by,
        )
        .await?;
        tracing::info!(item = %id, "memory item forgotten");
        Ok(())
    }

    /// Forgets every item matching `filter`; returns how many were forgotten.
    pub async fn forget_matching(&self, filter: &ForgetFilter, by: Actor) -> Result<usize> {
        let ids = self.ids_matching(filter).await?;
        let mut forgotten = 0;
        for id in ids {
            self.forget(id, by.clone()).await?;
            forgotten += 1;
        }
        Ok(forgotten)
    }

    // -- reads -------------------------------------------------------------

    /// Hybrid search: vector candidates ∪ lexical candidates, scored by
    /// [`crate::rank::hybrid_score`], deduplicated and truncated to `top_k`.
    ///
    /// The lexical leg derives its query from `websearch_to_tsquery` (as
    /// `plan/06` §1.4 specifies) but relaxes its implicit `AND` to `OR`:
    /// retrieval queries are whole goals or task instructions, and requiring
    /// *every* stem would make the lexical leg fire only on near-identical
    /// text. `ts_rank_cd` still ranks documents matching more stems higher.
    pub async fn search(&self, query: SearchQuery) -> Result<Vec<Hit>> {
        self.ensure_enabled()?;
        let started = std::time::Instant::now();
        let top_k = if query.top_k == 0 {
            self.cfg.top_k
        } else {
            query.top_k
        };
        let min_similarity = if query.min_similarity < 0.0 {
            self.cfg.min_similarity
        } else {
            query.min_similarity
        };
        let text = truncate_input(&query.text);
        let embedding = match embed_one(self.embedder.as_ref(), &text).await {
            Ok(vector) => Some(Vector::from(vector)),
            Err(EmbedError::Disabled) => None,
            Err(err) => {
                tracing::warn!(error = %err, "embedding the query failed; lexical search only");
                None
            }
        };
        let kinds: Option<Vec<String>> = (!query.kinds.is_empty())
            .then(|| query.kinds.iter().map(|k| k.as_str().to_owned()).collect());
        let tags: Option<Vec<String>> =
            (!query.tags_any.is_empty()).then(|| query.tags_any.clone());
        let candidate_limit = i64::try_from((top_k * 4).max(4)).unwrap_or(i64::MAX);

        let sql = format!(
            "WITH tsq AS (
                 SELECT CASE WHEN $2 = '' THEN NULL ELSE
                     replace(websearch_to_tsquery('english', $2)::text, '&', '|')::tsquery
                 END AS q
             ), vec AS (
                 SELECT id, (1 - (embedding <=> $1::vector))::float4 AS similarity
                 FROM memory.memory_items
                 WHERE {SEARCH_FILTERS} AND $1::vector IS NOT NULL AND embedding IS NOT NULL
                 ORDER BY embedding <=> $1::vector
                 LIMIT $6
             ), lex AS (
                 SELECT id, ts_rank_cd(tsv, (SELECT q FROM tsq))::float4 AS lexical
                 FROM memory.memory_items
                 WHERE {SEARCH_FILTERS} AND (SELECT q FROM tsq) IS NOT NULL
                       AND tsv @@ (SELECT q FROM tsq)
                 ORDER BY 2 DESC
                 LIMIT $6
             ), candidates AS (SELECT id FROM vec UNION SELECT id FROM lex)
             SELECT {ITEM_COLUMNS_QUALIFIED}, coalesce(v.similarity, 0)::float4 AS similarity,
                    coalesce(l.lexical, 0)::float4 AS lexical
             FROM memory.memory_items i
             JOIN candidates c ON c.id = i.id
             LEFT JOIN vec v ON v.id = i.id
             LEFT JOIN lex l ON l.id = i.id"
        );
        let rows = sqlx::query(AssertSqlSafe(sql))
            .bind(embedding)
            .bind(&text)
            .bind(kinds)
            .bind(tags)
            .bind(query.scope.scopes())
            .bind(candidate_limit)
            .fetch_all(&self.pool)
            .await?;

        let now = Utc::now();
        let raw_lexical: Vec<f32> = rows
            .iter()
            .map(|row| row.try_get::<f32, _>("lexical").unwrap_or(0.0))
            .collect();
        let normalised = rank::normalise_lexical(&raw_lexical);
        let mut hits = Vec::with_capacity(rows.len());
        for (row, lexical) in rows.iter().zip(normalised) {
            let item = row_to_item(row)?;
            let similarity = row.try_get::<f32, _>("similarity").unwrap_or(0.0);
            if similarity < min_similarity && lexical <= 0.0 {
                continue;
            }
            let decay = rank::decay(
                rank::age_days(item.created_at, now),
                self.cfg.decay_half_life_days,
            );
            let score = rank::hybrid_score(similarity, lexical, item.importance, decay);
            hits.push(Hit {
                item,
                similarity,
                lexical,
                score,
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.item.created_at.cmp(&a.item.created_at))
        });
        hits.truncate(top_k);
        metrics::histogram!(MEMORY_SEARCH_DURATION_SECONDS, "embedder" => self.cfg.embedder.as_str())
            .record(started.elapsed().as_secs_f64());
        Ok(hits)
    }

    /// One item by id (forgotten items included, for provenance).
    pub async fn get(&self, id: MemoryItemId) -> Result<Option<MemoryRecord>> {
        let sql = format!("SELECT {ITEM_COLUMNS} FROM memory.memory_items WHERE id = $1");
        let row = sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(row_to_item).transpose()
    }

    /// `memory.lessons_view`, newest first (`kevin lessons`).
    pub async fn lessons(&self, scope: &ScopeFilter, limit: usize) -> Result<Vec<Lesson>> {
        let rows = sqlx::query(
            "SELECT id, content, tags, scope, importance, run_id, created_at \
             FROM memory.lessons_view WHERE scope = ANY($1) ORDER BY created_at DESC LIMIT $2",
        )
        .bind(scope.scopes())
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Lesson {
                    id: MemoryItemId::from_uuid(row.try_get::<Uuid, _>("id")?),
                    content: row.try_get("content")?,
                    tags: row.try_get("tags")?,
                    scope: parse_scope(row.try_get::<String, _>("scope")?.as_str()),
                    importance: row.try_get("importance")?,
                    run_id: row.try_get("run_id")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    /// Counts per kind and embedding state (`kevin memory doctor`).
    pub async fn counts(&self) -> Result<MemoryCounts> {
        let rows = sqlx::query(
            "SELECT kind, count(*) AS n FROM memory.memory_items \
             WHERE forgotten_at IS NULL AND superseded_by IS NULL GROUP BY kind",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut counts = MemoryCounts::default();
        for row in &rows {
            let kind: String = row.try_get("kind")?;
            let n: i64 = row.try_get("n")?;
            counts.live += n;
            counts.by_kind.insert(kind, n);
        }
        counts.forgotten = sqlx::query_scalar(
            "SELECT count(*) FROM memory.memory_items WHERE forgotten_at IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        counts.pending_embedding = sqlx::query_scalar(
            "SELECT count(*) FROM memory.memory_items \
             WHERE forgotten_at IS NULL AND superseded_by IS NULL AND embedding IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        counts.models = sqlx::query_scalar(
            "SELECT DISTINCT embedding_model FROM memory.memory_items \
             WHERE embedding_model IS NOT NULL ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await?;
        #[allow(clippy::cast_precision_loss)] // a row count never exceeds 2^53
        metrics::gauge!(MEMORY_REINDEX_PENDING).set(counts.pending_embedding as f64);
        Ok(counts)
    }

    /// Width of the `embedding` column (`vector(N)`), read from the catalog.
    pub async fn column_dimensions(&self) -> Result<usize> {
        let modifier: Option<i32> = sqlx::query_scalar(
            "SELECT a.atttypmod FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'memory' AND c.relname = 'memory_items' AND a.attname = 'embedding'",
        )
        .fetch_optional(&self.pool)
        .await?;
        match modifier {
            Some(m) if m > 0 => Ok(usize::try_from(m).unwrap_or(0)),
            _ => Err(MemoryError::Invalid(
                "memory.memory_items.embedding is missing or has no declared width \
                 (is migration 0004_memory applied?)"
                    .to_owned(),
            )),
        }
    }

    /// Whether the HNSW index of the vector column exists (`doctor`).
    pub async fn hnsw_index_present(&self) -> Result<bool> {
        let present: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'memory' \
             AND tablename = 'memory_items' AND indexname = 'memory_items_embedding_hnsw')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(present)
    }

    // -- maintenance -------------------------------------------------------

    /// Recomputes embeddings for every live item whose `embedding_model` is
    /// not the current one, in batches of `batch`, reporting `(done, total)`.
    ///
    /// Resumable: a killed run leaves already-updated rows out of the next
    /// selection. A *dimension* change is a migration for the column plus this
    /// command; when the model's width no longer matches the column, this
    /// returns [`MemoryError::DimensionMismatch`] instead of writing garbage.
    pub async fn reindex(
        &self,
        batch: usize,
        mut progress: impl FnMut(usize, usize) + Send,
    ) -> Result<ReindexReport> {
        self.ensure_enabled()?;
        let model = self.embedder.model_name().to_owned();
        if model == NOOP_MODEL {
            return Err(MemoryError::Embed(EmbedError::Disabled));
        }
        let column = self.column_dimensions().await?;
        let produced = self.embedder.dimensions();
        if produced != column || produced != self.cfg.dimensions {
            return Err(MemoryError::DimensionMismatch {
                model,
                expected: column,
                actual: produced,
            });
        }
        let batch = batch.clamp(1, 512);
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memory.memory_items \
             WHERE forgotten_at IS NULL AND content <> '' AND embedding_model IS DISTINCT FROM $1",
        )
        .bind(&model)
        .fetch_one(&self.pool)
        .await?;
        let total = usize::try_from(total).unwrap_or(0);
        let mut done = 0usize;
        loop {
            let rows = sqlx::query(
                "SELECT id, content FROM memory.memory_items \
                 WHERE forgotten_at IS NULL AND content <> '' \
                 AND embedding_model IS DISTINCT FROM $1 ORDER BY created_at LIMIT $2",
            )
            .bind(&model)
            .bind(i64::try_from(batch).unwrap_or(i64::MAX))
            .fetch_all(&self.pool)
            .await?;
            if rows.is_empty() {
                break;
            }
            let ids: Vec<Uuid> = rows
                .iter()
                .map(|row| row.try_get::<Uuid, _>("id"))
                .collect::<std::result::Result<_, _>>()?;
            let contents: Vec<String> = rows
                .iter()
                .map(|row| {
                    row.try_get::<String, _>("content")
                        .map(|c| truncate_input(&c))
                })
                .collect::<std::result::Result<_, _>>()?;
            let vectors = self.embed_batch_timed(&contents).await?;
            for (id, vector) in ids.iter().zip(vectors) {
                if vector.len() != column {
                    return Err(MemoryError::DimensionMismatch {
                        model,
                        expected: column,
                        actual: vector.len(),
                    });
                }
                sqlx::query(
                    "UPDATE memory.memory_items SET embedding = $2, embedding_model = $3 WHERE id = $1",
                )
                .bind(id)
                .bind(Vector::from(vector))
                .bind(&model)
                .execute(&self.pool)
                .await?;
                done += 1;
            }
            progress(done, total);
        }
        tracing::info!(model = %model, embedded = done, "memory reindex finished");
        Ok(ReindexReport {
            embedded: done,
            total,
            model,
        })
    }

    /// Every item, for `kevin memory export` (embeddings excluded unless asked).
    pub async fn export(&self, include_embeddings: bool) -> Result<Vec<ExportItem>> {
        let sql = format!(
            "SELECT {ITEM_COLUMNS}{} FROM memory.memory_items ORDER BY created_at",
            if include_embeddings {
                ", embedding::text AS embedding_text"
            } else {
                ""
            }
        );
        let rows = sqlx::query(AssertSqlSafe(sql))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|row| {
                let embedding = if include_embeddings {
                    row.try_get::<Option<String>, _>("embedding_text")
                        .ok()
                        .flatten()
                        .and_then(|text| parse_vector_text(&text))
                } else {
                    None
                };
                Ok(ExportItem {
                    item: row_to_item(row)?,
                    embedding,
                })
            })
            .collect()
    }

    /// Restores exported items (same ids), for backup/restore.
    ///
    /// Existing ids are skipped; live content still passes the redaction gate
    /// and is re-embedded when the export carried no (or a differently sized)
    /// vector; forgotten rows are restored as forgotten, blank and vectorless.
    /// `superseded_by` links are re-applied in a second pass, so the order of
    /// the export does not matter.
    pub async fn import(&self, items: &[ExportItem]) -> Result<ImportReport> {
        self.ensure_enabled()?;
        let mut report = ImportReport::default();
        for entry in items {
            let item = &entry.item;
            let forgotten = item.forgotten_at.is_some();
            let (content, embedding) = if forgotten {
                (String::new(), None)
            } else {
                let content = match self.vet_content(&item.content) {
                    Ok(content) => content,
                    Err(err) if err.is_refusal() => {
                        report.refused.push((item.id, err.to_string()));
                        continue;
                    }
                    Err(err) => return Err(err),
                };
                let embedding = match &entry.embedding {
                    Some(vector) if vector.len() == self.cfg.dimensions => Some(vector.clone()),
                    _ => self.embed_for_storage(&content).await?,
                };
                (content, embedding)
            };
            let inserted = sqlx::query(
                "INSERT INTO memory.memory_items \
                 (id, kind, content, tags, source, scope, importance, embedding, embedding_model, \
                  created_at, forgotten_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) ON CONFLICT (id) DO NOTHING",
            )
            .bind(item.id.as_uuid())
            .bind(item.kind.as_str())
            .bind(&content)
            .bind(&item.tags)
            .bind(serde_json::to_value(&item.source)?)
            .bind(item.scope.to_string())
            .bind(item.importance)
            .bind(embedding.clone().map(Vector::from))
            .bind(
                embedding
                    .as_ref()
                    .map(|_| self.embedder.model_name().to_owned()),
            )
            .bind(item.created_at)
            .bind(item.forgotten_at)
            .execute(&self.pool)
            .await
            .map_err(|e| self.map_db_error(e, embedding.as_ref().map_or(0, Vec::len)))?;
            if inserted.rows_affected() == 0 {
                report.skipped += 1;
            } else {
                report.imported += 1;
            }
        }
        // Second pass: supersede links, now that every target exists.
        for entry in items {
            let Some(head) = entry.item.superseded_by else {
                continue;
            };
            sqlx::query(
                "UPDATE memory.memory_items SET superseded_by = $2 \
                 WHERE id = $1 AND superseded_by IS NULL \
                 AND EXISTS (SELECT 1 FROM memory.memory_items WHERE id = $2)",
            )
            .bind(entry.item.id.as_uuid())
            .bind(head.as_uuid())
            .execute(&self.pool)
            .await?;
        }
        Ok(report)
    }

    // -- internals ---------------------------------------------------------

    fn ensure_enabled(&self) -> Result<()> {
        if self.cfg.enabled {
            Ok(())
        } else {
            Err(MemoryError::Disabled)
        }
    }

    /// The redaction gate (`plan/09` §Memory privacy): content that matches a
    /// secret pattern is refused outright — memory keeps no partially masked
    /// secret and the caller learns why.
    fn vet_content(&self, content: &str) -> Result<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(MemoryError::EmptyContent);
        }
        let len = trimmed.chars().count();
        if len > MAX_CONTENT_CHARS {
            return Err(MemoryError::TooLong { len });
        }
        match self.redactor.redact_str(trimmed) {
            Cow::Borrowed(clean) => Ok(clean.to_owned()),
            Cow::Owned(redacted) => {
                let kinds = marker_kinds(&redacted);
                let markers: usize = kinds
                    .iter()
                    .map(|kind| kevin_telemetry::redact::marker(kind).len())
                    .sum();
                let kept = redacted.len().saturating_sub(markers);
                let removed = trimmed.len().saturating_sub(kept);
                let percent = removed.saturating_mul(100) / trimmed.len().max(1);
                Err(MemoryError::ContainsSecret {
                    kinds: kinds.join(", "),
                    percent: u8::try_from(percent.min(100)).unwrap_or(100),
                })
            }
        }
    }

    /// Embeds content for storage; `None` when embeddings are disabled or the
    /// backend failed (the item is stored anyway and `reindex` picks it up).
    async fn embed_for_storage(&self, content: &str) -> Result<Option<Vec<f32>>> {
        match self
            .embed_batch_timed(std::slice::from_ref(&content.to_owned()))
            .await
        {
            Ok(mut vectors) => match vectors.pop() {
                Some(vector) if vector.len() == self.cfg.dimensions => Ok(Some(vector)),
                Some(vector) => Err(MemoryError::DimensionMismatch {
                    model: self.embedder.model_name().to_owned(),
                    expected: self.cfg.dimensions,
                    actual: vector.len(),
                }),
                None => Ok(None),
            },
            Err(MemoryError::Embed(EmbedError::Disabled)) => Ok(None),
            Err(MemoryError::Embed(err)) => {
                tracing::warn!(
                    error = %err,
                    "embedding failed; storing the item without a vector (run `kevin memory reindex`)"
                );
                metrics::gauge!(MEMORY_REINDEX_PENDING).increment(1.0);
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    async fn embed_batch_timed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let started = std::time::Instant::now();
        let out = self.embedder.embed_batch(inputs).await;
        metrics::histogram!(EMBEDDING_DURATION_SECONDS, "embedder" => self.cfg.embedder.as_str())
            .record(started.elapsed().as_secs_f64());
        Ok(out?)
    }

    async fn insert(
        &self,
        id: MemoryItemId,
        req: &StoreRequest,
        content: &str,
        embedding: Option<&Vec<f32>>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO memory.memory_items \
             (id, kind, content, tags, source, scope, importance, embedding, embedding_model) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id.as_uuid())
        .bind(req.kind.as_str())
        .bind(content)
        .bind(&req.tags)
        .bind(serde_json::to_value(&req.source)?)
        .bind(req.scope.to_string())
        .bind(req.importance.clamp(0.0, 1.0))
        .bind(embedding.cloned().map(Vector::from))
        .bind(embedding.map(|_| self.embedder.model_name().to_owned()))
        .execute(&self.pool)
        .await
        .map_err(|e| self.map_db_error(e, embedding.map_or(0, Vec::len)))?;
        if embedding.is_none() {
            metrics::gauge!(MEMORY_REINDEX_PENDING).increment(1.0);
        }
        Ok(())
    }

    /// Turns pgvector's "expected N dimensions, not M" into the typed error
    /// (the column width is the source of truth, not `memory.dimensions`).
    fn map_db_error(&self, err: sqlx::Error, actual: usize) -> MemoryError {
        if let sqlx::Error::Database(db) = &err {
            let message = db.message().to_owned();
            if message.contains("dimensions")
                && let Some(expected) = first_number(&message)
            {
                return MemoryError::DimensionMismatch {
                    model: self.embedder.model_name().to_owned(),
                    expected,
                    actual,
                };
            }
        }
        MemoryError::Database(err)
    }

    async fn ids_matching(&self, filter: &ForgetFilter) -> Result<Vec<MemoryItemId>> {
        let ids: Vec<Uuid> = match filter {
            ForgetFilter::Id(id) => {
                sqlx::query_scalar("SELECT id FROM memory.memory_items WHERE id = $1")
                    .bind(id.as_uuid())
                    .fetch_all(&self.pool)
                    .await?
            }
            ForgetFilter::Run(run) => {
                sqlx::query_scalar(
                    "SELECT id FROM memory.memory_items \
                 WHERE forgotten_at IS NULL AND source->>'run_id' = $1",
                )
                .bind(run.to_string())
                .fetch_all(&self.pool)
                .await?
            }
            ForgetFilter::Repo(repo) => {
                sqlx::query_scalar(
                    "SELECT id FROM memory.memory_items WHERE forgotten_at IS NULL AND scope = $1",
                )
                .bind(format!("repo:{repo}"))
                .fetch_all(&self.pool)
                .await?
            }
            ForgetFilter::Before(when) => sqlx::query_scalar(
                "SELECT id FROM memory.memory_items WHERE forgotten_at IS NULL AND created_at < $1",
            )
            .bind(when)
            .fetch_all(&self.pool)
            .await?,
        };
        Ok(ids.into_iter().map(MemoryItemId::from_uuid).collect())
    }

    /// Appends a `memory.*` event when an event store is wired.
    ///
    /// Deviation from `plan/06` §1.3: the append is a second transaction (the
    /// frozen `EventStore` trait exposes no transaction to join). Insert and
    /// event are therefore eventually — not atomically — consistent; a failed
    /// append is logged and returned so the caller can retry.
    // TODO(ws-03): join the insert's transaction once `EventStore` can append
    // inside a caller-provided transaction.
    async fn emit(&self, id: MemoryItemId, event: &MemoryItemEvent, actor: Actor) -> Result<()> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        let stream = events::stream(id);
        let version = events.load_stream(&stream, 0).await?.len() as u64;
        events
            .append(&stream, version, &[events::new_event(id, event, actor)?])
            .await?;
        Ok(())
    }
}

/// Parses the stored scope; anything unparsable degrades to `global` (a row
/// is never dropped because of a malformed scope, it just stops being
/// repo-private — which is the safe direction only for reads, so the value is
/// also logged).
fn parse_scope(value: &str) -> MemoryScope {
    value.parse().unwrap_or_else(|_| {
        tracing::warn!(
            scope = value,
            "unparsable memory scope; treating it as global"
        );
        MemoryScope::Global
    })
}

fn row_to_item(row: &PgRow) -> Result<MemoryRecord> {
    let source: serde_json::Value = row.try_get("source")?;
    Ok(MemoryRecord {
        id: MemoryItemId::from_uuid(row.try_get::<Uuid, _>("id")?),
        kind: parse_kind(row.try_get::<String, _>("kind")?.as_str())
            .map_err(|e| MemoryError::Invalid(e.to_string()))?,
        content: row.try_get("content")?,
        tags: row.try_get("tags")?,
        source: serde_json::from_value(source)
            .unwrap_or_else(|_| MemorySource::from_actor(Actor::system("memory"))),
        scope: parse_scope(row.try_get::<String, _>("scope")?.as_str()),
        importance: row.try_get("importance")?,
        embedding_model: row.try_get("embedding_model")?,
        created_at: row.try_get("created_at")?,
        superseded_by: row
            .try_get::<Option<Uuid>, _>("superseded_by")?
            .map(MemoryItemId::from_uuid),
        forgotten_at: row.try_get("forgotten_at")?,
    })
}

/// The distinct `[REDACTED:<kind>]` kinds present in `text`, in order.
fn marker_kinds(text: &str) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(kevin_telemetry::redact::MARKER_PREFIX) {
        rest = &rest[start + kevin_telemetry::redact::MARKER_PREFIX.len()..];
        let Some(end) = rest.find(']') else { break };
        let kind = rest[..end].to_owned();
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
        rest = &rest[end + 1..];
    }
    kinds
}

/// The first integer in a message (`expected 384 dimensions, not 64` → 384).
fn first_number(message: &str) -> Option<usize> {
    let digits: String = message
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Parses pgvector's text form `[1,2,3]`.
fn parse_vector_text(text: &str) -> Option<Vec<f32>> {
    let inner = text.trim().strip_prefix('[')?.strip_suffix(']')?;
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner.split(',').map(|v| v.trim().parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_kinds_are_deduplicated_in_order() {
        let text = "a [REDACTED:bearer] b [REDACTED:aws_key] c [REDACTED:bearer]";
        assert_eq!(marker_kinds(text), vec!["bearer", "aws_key"]);
        assert!(marker_kinds("nothing here").is_empty());
    }

    #[test]
    fn dimension_errors_are_parsed_from_the_database_message() {
        assert_eq!(first_number("expected 384 dimensions, not 64"), Some(384));
        assert_eq!(first_number("no numbers here"), None);
    }

    #[test]
    fn vector_text_round_trips() {
        assert_eq!(parse_vector_text("[1,2.5,-3]"), Some(vec![1.0, 2.5, -3.0]));
        assert_eq!(parse_vector_text("[]"), Some(Vec::new()));
        assert_eq!(parse_vector_text("nope"), None);
    }

    #[test]
    fn search_query_defaults_defer_to_the_configuration() {
        let query = SearchQuery::new("goal");
        assert_eq!(query.top_k, 0);
        assert!(query.min_similarity < 0.0);
        let tuned = query.with_top_k(3).with_min_similarity(0.5);
        assert_eq!(tuned.top_k, 3);
        assert!((tuned.min_similarity - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn store_requests_carry_the_kind_default_importance() {
        assert!((StoreRequest::fact("x").importance - 0.6).abs() < f32::EPSILON);
        assert!((StoreRequest::lesson("x").importance - 0.5).abs() < f32::EPSILON);
        assert!(
            (StoreRequest::new(MemoryKind::Preference, "x").importance - 0.8).abs() < f32::EPSILON
        );
    }
}
