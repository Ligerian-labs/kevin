-- WS-22 — Kohral runtime contract (plan/08-kohral-runtime.md §2).
--
-- `kohral.runs_ledger` is the durable status a Kohral worker polls: it is
-- written synchronously at acceptance (same transaction as `run.started`) and
-- then maintained by `KohralLedgerProjection`. Kohral's turn invariants
-- (append-only `partial_output`, monotonic `seq`) are enforced here by only
-- ever appending to `partial_output` and incrementing `seq`.

CREATE SCHEMA IF NOT EXISTS kohral;

CREATE TABLE IF NOT EXISTS kohral.runs_ledger (
    idempotency_key text PRIMARY KEY,
    request_hash    char(64) NOT NULL,
    request_json    jsonb NOT NULL,
    run_id          uuid NOT NULL UNIQUE,
    session_id      text NOT NULL,
    session_key     text,
    model           text,
    status          text NOT NULL
        CHECK (status IN ('queued', 'running', 'stopping', 'completed', 'failed', 'cancelled')),
    partial_output  text NOT NULL DEFAULT '',
    seq             bigint NOT NULL DEFAULT 0 CHECK (seq >= 0),
    message_id      text NOT NULL,
    usage           jsonb NOT NULL DEFAULT '{}',
    error_code      text CHECK (error_code ~ '^[a-z][a-z0-9_]{1,63}$'),
    error           text,
    last_event      text,
    -- Highest `core.events.position` already folded into this row. Projections
    -- are replayable (`kevin db rebuild-projection`), and `partial_output` /
    -- `seq` are not idempotent under a second fold, so every projection write
    -- is guarded with `last_position < $position`.
    last_position   bigint NOT NULL DEFAULT 0,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS runs_ledger_session_created_idx
    ON kohral.runs_ledger (session_id, created_at);
CREATE INDEX IF NOT EXISTS runs_ledger_updated_idx
    ON kohral.runs_ledger (updated_at, run_id);

-- The `/api/sessions/{id}/messages` view Kohral reconciles against. Message
-- ids are stable (`umsg_<run_id>` for the turn's user message, the run's
-- `message_id` for the assistant one) so a re-poll never duplicates.
CREATE TABLE IF NOT EXISTS kohral.session_messages (
    message_id text PRIMARY KEY,
    session_id text NOT NULL,
    run_id     uuid NOT NULL,
    role       text NOT NULL CHECK (role IN ('user', 'assistant')),
    content    text NOT NULL,
    created_at timestamptz NOT NULL
);

CREATE INDEX IF NOT EXISTS session_messages_session_created_idx
    ON kohral.session_messages (session_id, created_at);
