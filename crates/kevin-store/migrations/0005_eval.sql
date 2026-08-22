-- 0005_eval: eval schema owned by kevin-evaluator (plan/06-memory-and-learning.md §3.5).
-- Both tables are projections of the `evaluation.*` events and are rebuildable
-- from `core.events`; the event stream stays the source of truth.

CREATE SCHEMA IF NOT EXISTS eval;

-- One recorded judge evaluation of a run or a task.
CREATE TABLE IF NOT EXISTS eval.evaluations (
    id            UUID        PRIMARY KEY,
    subject_type  TEXT        NOT NULL CHECK (subject_type IN ('run','task')),
    subject_id    UUID        NOT NULL,
    run_id        UUID        NOT NULL,
    attempt_id    UUID,
    rubric_id     TEXT        NOT NULL,
    judge_alias   TEXT        NOT NULL,
    judge_worker  TEXT        NOT NULL,
    scores        JSONB       NOT NULL,
    overall       REAL        NOT NULL CHECK (overall BETWEEN 0 AND 1),
    verdict       TEXT        NOT NULL CHECK (verdict IN ('accept','accept_with_fixes','reject')),
    lessons       TEXT[]      NOT NULL DEFAULT '{}',
    usage         JSONB       NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- `kevin eval show <subject>` and the run-level rollup read newest-first.
CREATE INDEX IF NOT EXISTS evaluations_subject ON eval.evaluations
    (subject_type, subject_id, created_at DESC);
-- Every evaluation of a run (cost rollup, `kevin eval rerun`).
CREATE INDEX IF NOT EXISTS evaluations_run ON eval.evaluations (run_id, created_at DESC);

-- The proposals inbox: prompt/config/routing changes a human accepts or rejects.
-- Never auto-applied (plan/adr/0010-evaluation-auto-apply-policy.md).
CREATE TABLE IF NOT EXISTS eval.proposals (
    id             UUID        PRIMARY KEY,
    evaluation_id  UUID        NOT NULL REFERENCES eval.evaluations (id),
    run_id         UUID        NOT NULL,
    kind           TEXT        NOT NULL CHECK (kind IN ('prompt','config','routing')),
    body           TEXT        NOT NULL,
    rationale      TEXT        NOT NULL DEFAULT '',
    status         TEXT        NOT NULL DEFAULT 'proposed' CHECK (status IN ('proposed','accepted','rejected')),
    decided_by     TEXT,
    decided_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- `kevin proposals ls` (the `eval.proposals_inbox` read model of plan/02 is this
-- table filtered on status = 'proposed').
CREATE INDEX IF NOT EXISTS proposals_inbox ON eval.proposals (status, created_at DESC);
CREATE INDEX IF NOT EXISTS proposals_evaluation ON eval.proposals (evaluation_id);
