-- 0001_core: platform schema owned by kevin-store (plan/01-architecture.md §Storage).
-- The `vector` extension is created here and only here; every later schema relies on it.

CREATE EXTENSION IF NOT EXISTS vector;

CREATE SCHEMA IF NOT EXISTS core;

-- Global, append-only event log. `position` is the global order (BIGSERIAL);
-- `(aggregate_type, aggregate_id, aggregate_version)` is the per-stream order.
CREATE TABLE IF NOT EXISTS core.events (
    position           BIGSERIAL    PRIMARY KEY,
    event_id           UUID         NOT NULL UNIQUE,
    event_type         TEXT         NOT NULL,
    schema_version     INTEGER      NOT NULL CHECK (schema_version >= 0),
    occurred_at        TIMESTAMPTZ  NOT NULL,
    aggregate_type     TEXT         NOT NULL,
    aggregate_id       UUID         NOT NULL,
    aggregate_version  BIGINT       NOT NULL CHECK (aggregate_version >= 1),
    correlation_id     UUID         NOT NULL,
    causation_id       UUID,
    actor              JSONB        NOT NULL,
    payload            JSONB        NOT NULL,
    recorded_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    CONSTRAINT events_stream_version_unique UNIQUE (aggregate_type, aggregate_id, aggregate_version)
);

CREATE INDEX IF NOT EXISTS events_correlation_idx ON core.events (correlation_id, position);
CREATE INDEX IF NOT EXISTS events_type_idx ON core.events (event_type, position);

-- Transactional outbox: one row per appended event, written in the same
-- transaction as the event. The relay delivers undelivered rows in position
-- order (at-least-once) and stamps `delivered_at`.
CREATE TABLE IF NOT EXISTS core.outbox (
    position      BIGINT       PRIMARY KEY REFERENCES core.events (position),
    event_id      UUID         NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    delivered_at  TIMESTAMPTZ,
    attempts      INTEGER      NOT NULL DEFAULT 0,
    last_error    TEXT
);

CREATE INDEX IF NOT EXISTS outbox_pending_idx ON core.outbox (position) WHERE delivered_at IS NULL;

-- Latest snapshot of an aggregate stream (one per stream, upserted).
CREATE TABLE IF NOT EXISTS core.snapshots (
    aggregate_type     TEXT         NOT NULL,
    aggregate_id       UUID         NOT NULL,
    aggregate_version  BIGINT       NOT NULL CHECK (aggregate_version >= 1),
    state              JSONB        NOT NULL,
    taken_at           TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (aggregate_type, aggregate_id)
);

-- Idempotency log: a retried command (same command_id) returns the original result.
CREATE TABLE IF NOT EXISTS core.processed_commands (
    command_id  UUID         PRIMARY KEY,
    result      JSONB        NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Durable per-projection position (last processed global position).
CREATE TABLE IF NOT EXISTS core.projection_checkpoints (
    name        TEXT         PRIMARY KEY,
    position    BIGINT       NOT NULL CHECK (position >= 0),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);
