-- WS-09 — routing schema (plan/06-memory-and-learning.md §2.1).
--
-- `routing.model_aliases` is the versioned snapshot of the `[models]` catalog
-- (one row set per `catalog_version`), `routing.route_scores` holds the
-- `RouteScore` aggregate state per `(task_kind, alias)` and
-- `routing.route_outcomes` is the append-only log of terminal task attempts.
-- Events in `core.events` remain the truth; these tables are snapshots/read
-- models, rebuildable from `routing.score_updated`.

CREATE SCHEMA IF NOT EXISTS routing;

CREATE TABLE IF NOT EXISTS routing.model_aliases (
    catalog_version  text NOT NULL,
    alias            text NOT NULL,
    worker           text NOT NULL,
    model            text NOT NULL,
    tier             text NOT NULL,
    context_tokens   bigint,
    input_usd_per_m  numeric,
    output_usd_per_m numeric,
    tags             text[] NOT NULL DEFAULT '{}',
    extra            jsonb NOT NULL DEFAULT '{}',
    first_seen       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (catalog_version, alias)
);

CREATE TABLE IF NOT EXISTS routing.route_scores (
    task_kind       text NOT NULL,
    alias           text NOT NULL,
    attempts        int NOT NULL DEFAULT 0,
    successes       int NOT NULL DEFAULT 0,
    -- Beta posterior, seeded from the tier prior (plan/06 §2.3).
    alpha           real NOT NULL,
    beta            real NOT NULL,
    -- Judge quality: EMA (used for ranking) plus the sums behind the mean.
    quality_ema     real,
    sum_quality     real NOT NULL DEFAULT 0,
    quality_samples int NOT NULL DEFAULT 0,
    -- Running sums over successful attempts; the plan's `mean_*` columns are
    -- derived from them so a mean never has to be re-derived by the caller.
    sum_cost_usd    numeric NOT NULL DEFAULT 0,
    cost_samples    int NOT NULL DEFAULT 0,
    sum_wall_ms     bigint NOT NULL DEFAULT 0,
    mean_cost_usd   numeric GENERATED ALWAYS AS (
        CASE WHEN cost_samples > 0 THEN sum_cost_usd / cost_samples END
    ) STORED,
    mean_wall_ms    bigint GENERATED ALWAYS AS (
        CASE WHEN successes > 0 THEN sum_wall_ms / successes END
    ) STORED,
    last_used       timestamptz,
    version         bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (task_kind, alias)
);

CREATE TABLE IF NOT EXISTS routing.route_outcomes (
    id              uuid PRIMARY KEY,
    -- Provenance of the attempt; NULL for outcomes recorded outside a run
    -- (operator commands, evaluator replays without attempt ids).
    run_id          uuid,
    task_id         uuid,
    attempt_id      uuid,
    task_kind       text NOT NULL,
    alias           text NOT NULL,
    catalog_version text NOT NULL,
    success         boolean NOT NULL,
    quality         real,
    cost_usd        numeric,
    wall_ms         bigint,
    failure_class   text,
    recorded_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS route_outcomes_kind_alias
    ON routing.route_outcomes (task_kind, alias, recorded_at DESC);
-- One outcome per attempt: re-recording the same attempt updates the row and
-- leaves `route_scores` untouched (idempotent re-evaluation, plan/06 §3.3).
CREATE UNIQUE INDEX IF NOT EXISTS route_outcomes_attempt
    ON routing.route_outcomes (attempt_id) WHERE attempt_id IS NOT NULL;

-- Read model behind `kevin routes` (plan/02 §Read models): the score row plus
-- the derived rates the leaderboard prints.
CREATE OR REPLACE VIEW routing.route_leaderboard AS
SELECT s.*,
       CASE WHEN s.attempts > 0 THEN s.successes::real / s.attempts::real ELSE 0::real END AS win_rate,
       s.alpha / (s.alpha + s.beta) AS p_success,
       CASE WHEN s.quality_samples > 0 THEN s.sum_quality / s.quality_samples::real END AS mean_quality
FROM routing.route_scores s;
