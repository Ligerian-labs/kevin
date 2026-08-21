//! WS-18 acceptance criteria (`plan/12-workstreams.md`): hybrid search, decay,
//! forget, redaction, reindex — all against a real Postgres + pgvector with a
//! deterministic [`FixedEmbedder`] (no model, no network).

use std::sync::Arc;

use kevin_domain::{Actor, MemoryItemId, MemoryKind, MemoryScope, MemorySource, RunId};
use kevin_memory::embed::FIXED_MODEL;
use kevin_memory::{
    ContextBuilder, EmbedderKind, FixedEmbedder, ForgetFilter, MemoryCfg, MemoryError, MemoryStore,
    NoopEmbedder, RepoId, ScopeFilter, SearchQuery, StoreRequest,
};
use kevin_store::{EventStore, PgEventStore, PgPool};
use kevin_testkit::pg::TestDb;

const DIMENSIONS: usize = 384;

fn cfg() -> MemoryCfg {
    MemoryCfg::default().with_embedder(EmbedderKind::None, FIXED_MODEL)
}

/// A store using the deterministic embedder (vectors are a hashed bag of
/// words, so "the planted item" really is the nearest neighbour).
fn fixed_store(pool: &PgPool) -> MemoryStore {
    MemoryStore::new(
        pool.clone(),
        Arc::new(FixedEmbedder::new(DIMENSIONS)),
        cfg(),
    )
}

fn user() -> MemorySource {
    MemorySource::from_actor(Actor::user("valentin"))
}

async fn backdate(pool: &PgPool, id: MemoryItemId, days: i32) {
    sqlx::query("UPDATE memory.memory_items SET created_at = now() - make_interval(days => $2) WHERE id = $1")
        .bind(id.as_uuid())
        .bind(days)
        .execute(pool)
        .await
        .expect("backdate");
}

// ---------------------------------------------------------------------------
// (1) hybrid search returns the planted item first
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws18_1_hybrid_search_returns_the_planted_item_first() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());

    let planted = store
        .store(
            StoreRequest::lesson("Run cargo fmt before opening pull requests in this repo")
                .with_source(user())
                .with_tags(["rust", "ci"]),
        )
        .await
        .expect("store planted lesson");
    for noise in [
        "The deploy stack uses podman compose on the VPS",
        "Ratatui screens are snapshot-tested with TestBackend",
        "Budgets are enforced per run and per task",
    ] {
        store
            .store(StoreRequest::lesson(noise).with_source(user()))
            .await
            .expect("store noise");
    }

    let hits = store
        .search(SearchQuery::new(
            "should I run cargo fmt before opening a pull request?",
        ))
        .await
        .expect("search");

    assert!(!hits.is_empty(), "the planted item must be retrieved");
    assert_eq!(
        hits[0].item.id, planted,
        "planted item ranks first: {hits:#?}"
    );
    assert!(hits[0].similarity > 0.0, "vector leg contributed");
    assert!(hits[0].lexical > 0.0, "lexical leg contributed");
    assert!(hits[0].score > hits.get(1).map_or(0.0, |h| h.score));
    assert_eq!(hits[0].item.tags, vec!["rust".to_owned(), "ci".to_owned()]);

    // Tag and kind filters restrict the candidate set.
    let filtered = store
        .search(
            SearchQuery::new("cargo fmt")
                .with_kinds([MemoryKind::Fact])
                .with_tags_any(["rust"]),
        )
        .await
        .expect("filtered search");
    assert!(filtered.is_empty(), "no fact was stored: {filtered:#?}");

    db.close().await;
}

// ---------------------------------------------------------------------------
// (2) decay lowers the rank of old items
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws18_2_decay_lowers_the_rank_of_old_items() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());
    let text = "Always run the migrations before starting the daemon";

    let old = store
        .store(StoreRequest::lesson(text).with_source(user()))
        .await
        .expect("store old");
    let fresh = store
        .store(StoreRequest::lesson(text).with_source(user()))
        .await
        .expect("store fresh");
    // Four half-lives (90 d each) in the past.
    backdate(db.pool(), old, 360).await;

    let hits = store.search(SearchQuery::new(text)).await.expect("search");
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].item.id, fresh,
        "the fresh item outranks the old one"
    );
    assert_eq!(hits[1].item.id, old);
    assert!(
        hits[0].score > hits[1].score,
        "decay must lower the old item's score: {hits:#?}"
    );
    // Only the importance term decays: the gap stays within its weight.
    assert!(hits[0].score - hits[1].score <= kevin_memory::W_IMPORTANCE * 0.5 + 1e-6);
    // Nothing is deleted by age.
    assert!(store.get(old).await.expect("get").is_some());

    db.close().await;
}

// ---------------------------------------------------------------------------
// (3) forget removes an item from search and marks it forgotten
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws18_3_forget_removes_the_item_from_search() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());
    let run = RunId::new();

    let id = store
        .store(
            StoreRequest::fact("The staging database lives on port 5433").with_source(
                MemorySource {
                    run_id: Some(run),
                    ..user()
                },
            ),
        )
        .await
        .expect("store");
    assert_eq!(
        store
            .search(SearchQuery::new("staging database port"))
            .await
            .expect("search before")
            .len(),
        1
    );

    store
        .forget(id, Actor::user("valentin"))
        .await
        .expect("forget");

    assert!(
        store
            .search(SearchQuery::new("staging database port"))
            .await
            .expect("search after")
            .is_empty(),
        "a forgotten item is never retrieved"
    );
    let row = store.get(id).await.expect("get").expect("row is kept");
    assert!(row.forgotten_at.is_some(), "forgotten_at is stamped");
    assert!(row.content.is_empty(), "content is blanked");
    assert!(row.embedding_model.is_none(), "the vector is dropped");
    assert_eq!(row.source.run_id, Some(run), "provenance is kept");
    // Idempotent, and unknown ids are reported.
    store
        .forget(id, Actor::user("valentin"))
        .await
        .expect("forget twice is a no-op");
    assert!(matches!(
        store.forget(MemoryItemId::new(), Actor::user("v")).await,
        Err(MemoryError::NotFound(_))
    ));

    // `--run`: everything learned during a run.
    for text in ["Learned A during the run", "Learned B during the run"] {
        store
            .store(StoreRequest::lesson(text).with_source(MemorySource {
                run_id: Some(run),
                ..user()
            }))
            .await
            .expect("store run item");
    }
    let forgotten = store
        .forget_matching(&ForgetFilter::Run(run), Actor::user("valentin"))
        .await
        .expect("forget by run");
    assert_eq!(forgotten, 2);
    assert!(
        store
            .search(SearchQuery::new("learned during the run"))
            .await
            .expect("search")
            .is_empty()
    );

    db.close().await;
}

// ---------------------------------------------------------------------------
// (4) redaction refuses to store secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws18_4_redaction_refuses_content_with_an_api_key() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());

    for secret in [
        "The Anthropic key is sk-ant-api03-abcdefghijklmnopqrstuvwxyz012345",
        "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE for the deploy",
        "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig'",
        "connect with postgres://kevin:s3cr3tpassword@db.internal:5432/kevin",
    ] {
        let err = store
            .store(StoreRequest::fact(secret).with_source(user()))
            .await
            .expect_err("content with a secret must be refused");
        assert!(
            matches!(err, MemoryError::ContainsSecret { .. }),
            "expected ContainsSecret, got {err:?}"
        );
        assert!(err.is_refusal());
    }

    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM memory.memory_items")
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(stored, 0, "nothing with a secret reaches the table");

    // Innocent content still goes through, empty content does not.
    store
        .store(StoreRequest::fact("Kevin holds no provider API keys").with_source(user()))
        .await
        .expect("clean content is stored");
    assert!(matches!(
        store
            .store(StoreRequest::fact("   ").with_source(user()))
            .await,
        Err(MemoryError::EmptyContent)
    ));

    db.close().await;
}

// ---------------------------------------------------------------------------
// (5) reindex: recompute for the current model, refuse a dimension change
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ac_ws18_5_reindex_recomputes_embeddings_and_rejects_a_dimension_change() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let old_model = MemoryStore::new(
        db.pool().clone(),
        Arc::new(FixedEmbedder::named(DIMENSIONS, "fixed-model-a")),
        cfg(),
    );
    for text in ["First lesson", "Second lesson", "Third lesson"] {
        old_model
            .store(StoreRequest::lesson(text).with_source(user()))
            .await
            .expect("store");
    }
    // An item stored while the embedder was down (no vector at all).
    let pending = MemoryStore::new(
        db.pool().clone(),
        Arc::new(NoopEmbedder::new(DIMENSIONS)),
        cfg(),
    )
    .store(StoreRequest::lesson("Stored without a vector").with_source(user()))
    .await
    .expect("store without embedding");
    assert!(
        old_model
            .get(pending)
            .await
            .expect("get")
            .expect("row")
            .embedding_model
            .is_none()
    );

    // New model, same width: every item is re-embedded, resumably.
    let new_model = MemoryStore::new(
        db.pool().clone(),
        Arc::new(FixedEmbedder::named(DIMENSIONS, "fixed-model-b")),
        cfg(),
    );
    let mut progress = Vec::new();
    let report = new_model
        .reindex(2, |done, total| progress.push((done, total)))
        .await
        .expect("reindex");
    assert_eq!(report.total, 4);
    assert_eq!(report.embedded, 4);
    assert_eq!(report.model, "fixed-model-b");
    assert_eq!(progress.last(), Some(&(4, 4)), "progress is reported");
    let models: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT embedding_model FROM memory.memory_items WHERE embedding IS NOT NULL",
    )
    .fetch_all(db.pool())
    .await
    .expect("models");
    assert_eq!(models, vec!["fixed-model-b".to_owned()]);

    // Resumable/idempotent: a second run has nothing left to do.
    let again = new_model
        .reindex(2, |_, _| {})
        .await
        .expect("reindex again");
    assert_eq!(again.embedded, 0);
    assert_eq!(again.total, 0);

    // A dimension change is a migration + reindex: without the migration the
    // command refuses instead of writing vectors of the wrong width.
    let other_width = MemoryStore::new(
        db.pool().clone(),
        Arc::new(FixedEmbedder::named(64, "fixed-model-small")),
        cfg().with_dimensions(64),
    );
    let err = other_width
        .reindex(2, |_, _| {})
        .await
        .expect_err("dimension change must be refused");
    match err {
        MemoryError::DimensionMismatch {
            expected, actual, ..
        } => {
            assert_eq!(expected, DIMENSIONS);
            assert_eq!(actual, 64);
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }
    assert_eq!(
        new_model.column_dimensions().await.expect("column width"),
        DIMENSIONS
    );
    // Storing with a mismatched embedder is refused for the same reason.
    assert!(matches!(
        other_width
            .store(StoreRequest::fact("mismatch").with_source(user()))
            .await,
        Err(MemoryError::DimensionMismatch { .. })
    ));

    db.close().await;
}

// ---------------------------------------------------------------------------
// Supporting behaviour (plan/06 §1.3–1.8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scopes_isolate_repositories_and_supersede_keeps_only_the_head() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());
    let kevin = RepoId::from_origin("https://github.com/Ligerian-labs/kevin");
    let other = RepoId::from_origin("https://github.com/acme/widgets");

    let old = store
        .store(
            StoreRequest::lesson("Use jj, never git checkout, in this repository")
                .with_scope(kevin.scope())
                .with_source(user()),
        )
        .await
        .expect("store repo lesson");
    store
        .store(
            StoreRequest::lesson("Use npm workspaces in this repository")
                .with_scope(other.scope())
                .with_source(user()),
        )
        .await
        .expect("store other repo lesson");
    store
        .store(
            StoreRequest::fact("Conventional commits everywhere")
                .with_scope(MemoryScope::Global)
                .with_source(user()),
        )
        .await
        .expect("store global fact");

    let hits = store
        .search(
            SearchQuery::new("which repository tooling should I use")
                .with_scope(ScopeFilter::RepoAndGlobal(kevin.clone())),
        )
        .await
        .expect("search");
    let scopes: Vec<String> = hits.iter().map(|h| h.item.scope.to_string()).collect();
    assert!(
        scopes
            .iter()
            .all(|s| s == "global" || s == &kevin.scope().to_string())
    );
    assert!(
        !scopes.contains(&other.scope().to_string()),
        "another repository's items are never retrieved: {scopes:?}"
    );

    // Superseding keeps only the head of the chain in search results.
    let new = store
        .supersede(
            old,
            StoreRequest::lesson(
                "Use jj bookmarks named type/short-description in this repository",
            )
            .with_scope(kevin.scope())
            .with_source(user()),
        )
        .await
        .expect("supersede");
    let hits = store
        .search(SearchQuery::new("jj in this repository").with_scope(ScopeFilter::Repo(kevin)))
        .await
        .expect("search after supersede");
    let ids: Vec<MemoryItemId> = hits.iter().map(|h| h.item.id).collect();
    assert!(ids.contains(&new), "the head is retrieved");
    assert!(!ids.contains(&old), "the superseded item is not: {ids:?}");
    assert_eq!(
        store
            .get(old)
            .await
            .expect("get")
            .expect("row")
            .superseded_by,
        Some(new)
    );

    db.close().await;
}

#[tokio::test]
async fn without_an_embedder_search_still_returns_lexical_hits() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = MemoryStore::new(
        db.pool().clone(),
        Arc::new(NoopEmbedder::new(DIMENSIONS)),
        cfg(),
    );

    let id = store
        .store(
            StoreRequest::fact("Prometheus metrics are served on 127.0.0.1:9464")
                .with_source(user()),
        )
        .await
        .expect("store");
    let row = store.get(id).await.expect("get").expect("row");
    assert!(row.embedding_model.is_none(), "stored with embedding NULL");

    let hits = store
        .search(SearchQuery::new("prometheus metrics"))
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item.id, id);
    assert!(hits[0].similarity.abs() < f32::EPSILON, "no vector leg");
    assert!(hits[0].lexical > 0.0, "lexical leg carries the hit");

    let counts = store.counts().await.expect("counts");
    assert_eq!(counts.live, 1);
    assert_eq!(counts.pending_embedding, 1);
    assert!(store.hnsw_index_present().await.expect("index check"));

    db.close().await;
}

#[tokio::test]
async fn the_intake_context_block_is_rendered_and_capped() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());
    let repo = RepoId::from_origin("https://github.com/Ligerian-labs/kevin");

    store
        .store(
            StoreRequest::lesson("Run cargo fmt before opening PRs in this repo")
                .with_scope(repo.scope())
                .with_source(user()),
        )
        .await
        .expect("lesson");
    store
        .store(
            StoreRequest::new(
                MemoryKind::Preference,
                "User prefers PRs opened from jj bookmarks named type/short-description",
            )
            .with_source(user()),
        )
        .await
        .expect("preference");
    store
        .store(
            StoreRequest::new(
                MemoryKind::RunSummary,
                "Opened a PR that formatted the code with cargo fmt and added the event store",
            )
            .with_scope(repo.scope())
            .with_source(user()),
        )
        .await
        .expect("run summary");

    let block = ContextBuilder::new(&store)
        .for_intake("open a PR that formats the code", Some(&repo))
        .await
        .expect("intake context");
    assert!(block.text.starts_with("<kevin-memory>"));
    assert!(block.text.ends_with("</kevin-memory>"));
    assert!(block.text.contains("Lessons (most relevant first):"));
    assert!(!block.refs.is_empty());
    assert!(block.estimated_tokens <= store.cfg().context_max_tokens);

    assert!(
        block.refs.len() >= 2,
        "several kinds are retrieved: {}",
        block.text
    );

    // A tighter cap drops the lowest-scoring hits rather than truncating text.
    let cap = block.estimated_tokens - 1;
    let tight = ContextBuilder::new(&store)
        .with_max_tokens(cap)
        .for_intake("open a PR that formats the code", Some(&repo))
        .await
        .expect("capped context");
    assert!(tight.estimated_tokens <= cap, "{}", tight.text);
    assert!(tight.refs.len() < block.refs.len(), "{}", tight.text);
    assert!(!tight.is_empty(), "the best hits still fit");
    assert!(tight.text.ends_with("</kevin-memory>"));
    assert_eq!(tight.refs.first(), block.refs.first(), "best hit is kept");

    db.close().await;
}

#[tokio::test]
async fn writes_append_memory_events_and_export_import_round_trips() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let events = Arc::new(PgEventStore::new(db.pool().clone()));
    let store = fixed_store(db.pool()).with_events(events.clone());

    let id = store
        .store(StoreRequest::lesson("Prefer nextest over cargo test").with_source(user()))
        .await
        .expect("store");
    // Export excludes embeddings by default; import restores into a fresh db.
    let exported = store.export(false).await.expect("export");
    assert_eq!(exported.len(), 1);
    assert!(exported[0].embedding.is_none());

    let other = TestDb::new().await;
    let restored = fixed_store(other.pool());
    let report = restored.import(&exported).await.expect("import");
    assert_eq!(report.imported, 1);
    assert!(report.refused.is_empty());
    let again = restored.import(&exported).await.expect("import twice");
    assert_eq!(again.skipped, 1, "ids already present are skipped");
    assert_eq!(
        restored
            .search(SearchQuery::new("nextest"))
            .await
            .expect("search restored")
            .len(),
        1,
        "restored items are searchable"
    );
    other.close().await;

    store
        .forget(id, Actor::user("valentin"))
        .await
        .expect("forget");

    let stream = kevin_memory::events::stream(id);
    let stored = events.load_stream(&stream, 0).await.expect("load stream");
    let types: Vec<&str> = stored.iter().map(|e| e.envelope.event_type).collect();
    assert_eq!(types, vec!["memory.item_stored", "memory.item_forgotten"]);
    assert_eq!(stored[0].envelope.payload["kind"], "lesson");

    // A forgotten row exports and re-imports as forgotten (provenance only).
    let after_forget = store.export(false).await.expect("export after forget");
    assert!(after_forget[0].item.forgotten_at.is_some());
    let third = TestDb::new().await;
    let restored = fixed_store(third.pool());
    let report = restored
        .import(&after_forget)
        .await
        .expect("import forgotten");
    assert_eq!(report.imported, 1);
    assert!(report.refused.is_empty(), "a blanked row is not a refusal");
    assert!(
        restored
            .search(SearchQuery::new("nextest"))
            .await
            .expect("search")
            .is_empty()
    );
    third.close().await;

    db.close().await;
}

#[tokio::test]
async fn lessons_view_lists_live_lessons_with_their_run() {
    kevin_testkit::skip_unless_pg!();
    let db = TestDb::new().await;
    let store = fixed_store(db.pool());
    let run = RunId::new();

    let kept = store
        .store(
            StoreRequest::lesson("Judge output must be recomputed server-side").with_source(
                MemorySource {
                    run_id: Some(run),
                    ..user()
                },
            ),
        )
        .await
        .expect("lesson");
    let forgotten = store
        .store(StoreRequest::lesson("This lesson will be forgotten").with_source(user()))
        .await
        .expect("lesson");
    store
        .store(StoreRequest::fact("Facts are not lessons").with_source(user()))
        .await
        .expect("fact");
    store
        .forget(forgotten, Actor::system("test"))
        .await
        .expect("forget");

    let lessons = store
        .lessons(&ScopeFilter::Global, 10)
        .await
        .expect("lessons view");
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].id, kept);
    assert_eq!(lessons[0].run_id.as_deref(), Some(run.to_string().as_str()));

    db.close().await;
}
