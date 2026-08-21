-- 0002_orch: read models owned by kevin-orchestrator's projections
-- (plan/01-architecture.md §Storage, plan/02-domain-model.md §Read models).
--
-- Every table here is a *projection* of `core.events`: it can be truncated and
-- rebuilt at any time (`kevin db rebuild-projection`). Rows carry the
-- aggregate `version` (or a `source_event_id`) so replaying an event twice is a
-- no-op, and `last_position` so operators can see how far a row has been
-- advanced. Money is `NUMERIC` and always read back as text (`cost_usd::text`)
-- so exactness survives the round trip.

CREATE SCHEMA IF NOT EXISTS orch;

-- ---------------------------------------------------------------------------
-- run_overview — `run.*` → RunDto / RunSummaryDto (plan/07 §DTOs)
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.run_overview (
    run_id               UUID         PRIMARY KEY,
    version              BIGINT       NOT NULL CHECK (version >= 1),
    status               TEXT         NOT NULL,
    goal_text            TEXT         NOT NULL,
    goal_excerpt         TEXT         NOT NULL,
    cwd                  TEXT         NOT NULL,
    repo_kind            TEXT         NOT NULL,
    mode                 TEXT         NOT NULL,
    mode_detail          JSONB,
    requested_by         TEXT         NOT NULL,
    auto_approve_plans   BOOLEAN      NOT NULL DEFAULT FALSE,
    budget               JSONB        NOT NULL DEFAULT '{}'::jsonb,
    usage                JSONB        NOT NULL DEFAULT '{}'::jsonb,
    cost_usd             NUMERIC,
    input_tokens         BIGINT       NOT NULL DEFAULT 0,
    output_tokens        BIGINT       NOT NULL DEFAULT 0,
    cache_read_tokens    BIGINT       NOT NULL DEFAULT 0,
    cache_write_tokens   BIGINT       NOT NULL DEFAULT 0,
    wall_ms              BIGINT       NOT NULL DEFAULT 0,
    planner_route        JSONB,
    understanding        JSONB,
    plan                 JSONB,
    plan_revision        INTEGER      NOT NULL DEFAULT 0,
    open_question_ids    UUID[]       NOT NULL DEFAULT '{}',
    task_ids             UUID[]       NOT NULL DEFAULT '{}',
    tasks_total          INTEGER      NOT NULL DEFAULT 0,
    tasks_succeeded      INTEGER      NOT NULL DEFAULT 0,
    tasks_failed         INTEGER      NOT NULL DEFAULT 0,
    tasks_cancelled      INTEGER      NOT NULL DEFAULT 0,
    tasks_skipped        INTEGER      NOT NULL DEFAULT 0,
    evaluation_id        UUID,
    evaluation_overall   REAL,
    evaluation_verdict   TEXT,
    budget_exhausted     TEXT,
    failure_reason       TEXT,
    failure_class        TEXT,
    failure_message      TEXT,
    cancelled_by         TEXT,
    cancel_reason        TEXT,
    summary              TEXT,
    artifacts            JSONB        NOT NULL DEFAULT '[]'::jsonb,
    created_at           TIMESTAMPTZ  NOT NULL,
    updated_at           TIMESTAMPTZ  NOT NULL,
    last_position        BIGINT       NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS run_overview_status_idx ON orch.run_overview (status, updated_at DESC);
CREATE INDEX IF NOT EXISTS run_overview_updated_idx ON orch.run_overview (updated_at DESC, run_id DESC);
CREATE INDEX IF NOT EXISTS run_overview_created_idx ON orch.run_overview (created_at DESC, run_id DESC);

-- ---------------------------------------------------------------------------
-- task_board — `task.*` → TaskDto / TaskSummaryDto, `attempts` = [AttemptDto]
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.task_board (
    task_id              UUID         PRIMARY KEY,
    run_id               UUID         NOT NULL,
    version              BIGINT       NOT NULL CHECK (version >= 1),
    seq                  INTEGER      NOT NULL DEFAULT 0,
    kind                 TEXT         NOT NULL,
    title                TEXT         NOT NULL,
    instructions         TEXT         NOT NULL DEFAULT '',
    status               TEXT         NOT NULL,
    spec                 JSONB        NOT NULL DEFAULT '{}'::jsonb,
    acceptance_criteria  JSONB        NOT NULL DEFAULT '[]'::jsonb,
    depends_on           UUID[]       NOT NULL DEFAULT '{}',
    budget               JSONB        NOT NULL DEFAULT '{}'::jsonb,
    route                JSONB,
    route_worker         TEXT,
    route_model          TEXT,
    route_effort         TEXT,
    selection            JSONB,
    attempts             JSONB        NOT NULL DEFAULT '[]'::jsonb,
    attempt_count        INTEGER      NOT NULL DEFAULT 0,
    usage                JSONB        NOT NULL DEFAULT '{}'::jsonb,
    cost_usd             NUMERIC,
    input_tokens         BIGINT       NOT NULL DEFAULT 0,
    output_tokens        BIGINT       NOT NULL DEFAULT 0,
    cache_read_tokens    BIGINT       NOT NULL DEFAULT 0,
    cache_write_tokens   BIGINT       NOT NULL DEFAULT 0,
    wall_ms              BIGINT       NOT NULL DEFAULT 0,
    artifacts            JSONB        NOT NULL DEFAULT '[]'::jsonb,
    summary              TEXT,
    failure_class        TEXT,
    failure_message      TEXT,
    awaiting_question_id UUID,
    started_at           TIMESTAMPTZ,
    ended_at             TIMESTAMPTZ,
    created_at           TIMESTAMPTZ  NOT NULL,
    updated_at           TIMESTAMPTZ  NOT NULL,
    last_position        BIGINT       NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS task_board_run_idx ON orch.task_board (run_id, seq, created_at);
CREATE INDEX IF NOT EXISTS task_board_status_idx ON orch.task_board (status, updated_at DESC);
CREATE INDEX IF NOT EXISTS task_board_updated_idx ON orch.task_board (updated_at DESC, task_id DESC);

-- ---------------------------------------------------------------------------
-- question_inbox — `question.*` → QuestionDto
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.question_inbox (
    question_id     UUID         PRIMARY KEY,
    run_id          UUID         NOT NULL,
    task_id         UUID,
    version         BIGINT       NOT NULL CHECK (version >= 1),
    text            TEXT         NOT NULL,
    options         JSONB        NOT NULL DEFAULT '[]'::jsonb,
    multi_select    BOOLEAN      NOT NULL DEFAULT FALSE,
    default_answer  JSONB,
    policy          JSONB        NOT NULL DEFAULT '{}'::jsonb,
    policy_kind     TEXT         NOT NULL DEFAULT 'block',
    timeout_ms      BIGINT,
    status          TEXT         NOT NULL,
    answer          JSONB,
    answered_by     TEXT,
    applied_default BOOLEAN      NOT NULL DEFAULT FALSE,
    asked_at        TIMESTAMPTZ  NOT NULL,
    answered_at     TIMESTAMPTZ,
    updated_at      TIMESTAMPTZ  NOT NULL,
    last_position   BIGINT       NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS question_inbox_run_idx ON orch.question_inbox (run_id, asked_at);
CREATE INDEX IF NOT EXISTS question_inbox_status_idx ON orch.question_inbox (status, asked_at DESC, question_id DESC);
CREATE INDEX IF NOT EXISTS question_inbox_updated_idx ON orch.question_inbox (updated_at DESC, question_id DESC);

-- ---------------------------------------------------------------------------
-- cost_ledger — one row per task attempt (`task.attempt_*`) → CostReportDto
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.cost_ledger (
    attempt_id          UUID         PRIMARY KEY,
    run_id              UUID         NOT NULL,
    task_id             UUID         NOT NULL,
    attempt_no          INTEGER      NOT NULL,
    version             BIGINT       NOT NULL CHECK (version >= 1),
    task_kind           TEXT         NOT NULL DEFAULT 'unknown',
    worker              TEXT         NOT NULL DEFAULT 'unknown',
    model_alias         TEXT         NOT NULL DEFAULT 'unknown',
    effort              TEXT,
    status              TEXT         NOT NULL,
    failure_class       TEXT,
    input_tokens        BIGINT       NOT NULL DEFAULT 0,
    output_tokens       BIGINT       NOT NULL DEFAULT 0,
    cache_read_tokens   BIGINT       NOT NULL DEFAULT 0,
    cache_write_tokens  BIGINT       NOT NULL DEFAULT 0,
    cost_usd            NUMERIC,
    wall_ms             BIGINT       NOT NULL DEFAULT 0,
    started_at          TIMESTAMPTZ  NOT NULL,
    ended_at            TIMESTAMPTZ,
    updated_at          TIMESTAMPTZ  NOT NULL,
    last_position       BIGINT       NOT NULL DEFAULT 0,
    CONSTRAINT cost_ledger_attempt_unique UNIQUE (task_id, attempt_no)
);

CREATE INDEX IF NOT EXISTS cost_ledger_run_idx ON orch.cost_ledger (run_id, started_at);
CREATE INDEX IF NOT EXISTS cost_ledger_started_idx ON orch.cost_ledger (started_at DESC, attempt_id DESC);
CREATE INDEX IF NOT EXISTS cost_ledger_model_idx ON orch.cost_ledger (model_alias, started_at DESC);
CREATE INDEX IF NOT EXISTS cost_ledger_kind_idx ON orch.cost_ledger (task_kind, started_at DESC);

-- ---------------------------------------------------------------------------
-- task_log — append-only worker transcript + task lifecycle lines
-- (plan/01 §Worker streams are not domain events) → TaskLogLineDto.
-- `seq` is assigned per `(task_id, attempt)` and is strictly monotonic;
-- `source_event_id` makes projection-written lines idempotent on replay.
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.task_log (
    task_id          UUID         NOT NULL,
    attempt          INTEGER      NOT NULL,
    seq              BIGINT       NOT NULL CHECK (seq >= 1),
    at               TIMESTAMPTZ  NOT NULL,
    kind             TEXT         NOT NULL,
    payload          JSONB        NOT NULL DEFAULT '{}'::jsonb,
    run_id           UUID,
    attempt_id       UUID,
    source_event_id  UUID,
    PRIMARY KEY (task_id, attempt, seq)
);

CREATE UNIQUE INDEX IF NOT EXISTS task_log_source_event_idx
    ON orch.task_log (source_event_id) WHERE source_event_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS task_log_at_idx ON orch.task_log (at);
CREATE INDEX IF NOT EXISTS task_log_run_idx ON orch.task_log (run_id, at);

-- ---------------------------------------------------------------------------
-- artifacts — every `ArtifactRef` seen on run/task events → ArtifactDto
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS orch.artifacts (
    artifact_id    UUID         PRIMARY KEY,
    run_id         UUID         NOT NULL,
    task_id        UUID,
    attempt_id     UUID,
    kind           TEXT         NOT NULL,
    uri            TEXT         NOT NULL,
    sha256         TEXT,
    bytes          BIGINT,
    content        BYTEA,
    produced_by    TEXT         NOT NULL DEFAULT 'task',
    created_at     TIMESTAMPTZ  NOT NULL,
    updated_at     TIMESTAMPTZ  NOT NULL,
    last_position  BIGINT       NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS artifacts_run_idx ON orch.artifacts (run_id, created_at);
CREATE INDEX IF NOT EXISTS artifacts_task_idx ON orch.artifacts (task_id, created_at);
