-- 0004_memory: memory schema owned by kevin-memory (plan/06-memory-and-learning.md §1.1).
-- The `vector` extension is created by 0001_core.sql; this file only uses it.

CREATE SCHEMA IF NOT EXISTS memory;

-- One durable memory item (lesson, preference, fact, run/artifact summary).
-- This table is both the aggregate state and the read model (deliberate
-- simplification documented in plan/06 §1.3).
CREATE TABLE IF NOT EXISTS memory.memory_items (
    id               UUID         PRIMARY KEY,
    kind             TEXT         NOT NULL CHECK (kind IN ('lesson','preference','fact','run_summary','artifact_summary')),
    content          TEXT         NOT NULL CHECK (length(content) <= 8000),
    tags             TEXT[]       NOT NULL DEFAULT '{}',
    source           JSONB        NOT NULL DEFAULT '{}',
    scope            TEXT         NOT NULL DEFAULT 'global',
    importance       REAL         NOT NULL DEFAULT 0.5 CHECK (importance BETWEEN 0 AND 1),
    embedding        VECTOR(384),
    embedding_model  TEXT,
    tsv              TSVECTOR     GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    created_at       TIMESTAMPTZ  NOT NULL DEFAULT now(),
    superseded_by    UUID         REFERENCES memory.memory_items (id),
    forgotten_at     TIMESTAMPTZ
);

-- Approximate nearest neighbour over cosine distance (pgvector HNSW).
CREATE INDEX IF NOT EXISTS memory_items_embedding_hnsw ON memory.memory_items
    USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64);
CREATE INDEX IF NOT EXISTS memory_items_tsv_gin ON memory.memory_items USING gin (tsv);
CREATE INDEX IF NOT EXISTS memory_items_tags_gin ON memory.memory_items USING gin (tags);
CREATE INDEX IF NOT EXISTS memory_items_scope_kind ON memory.memory_items (scope, kind)
    WHERE forgotten_at IS NULL AND superseded_by IS NULL;
-- `reindex` scans by embedding model; keep that scan cheap.
CREATE INDEX IF NOT EXISTS memory_items_embedding_model ON memory.memory_items (embedding_model)
    WHERE forgotten_at IS NULL;

-- Read model behind `kevin lessons` and the planner context.
CREATE OR REPLACE VIEW memory.lessons_view AS
    SELECT id, content, tags, scope, importance, source->>'run_id' AS run_id, created_at
    FROM memory.memory_items
    WHERE kind = 'lesson' AND forgotten_at IS NULL AND superseded_by IS NULL;
