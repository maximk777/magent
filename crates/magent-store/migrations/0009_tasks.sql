-- SDD: the task list a plan produces, and the review cycle that closes each
-- task out. Continues 0007_sdd.sql — a change reaches 'planned' when its
-- tasks exist, and 'ready' when they are all done.

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    change_id       TEXT NOT NULL REFERENCES sdd_changes(id) ON DELETE CASCADE,
    -- TEXT, not INTEGER: a plan is numbered hierarchically ("1.2", "3.10"),
    -- not sequentially. Uniqueness is scoped to the change so two tasks in
    -- one plan cannot claim the same number, and a run binds to its task by
    -- this number rather than by re-deriving position from list order.
    number          TEXT NOT NULL,
    title           TEXT NOT NULL,
    body            TEXT,
    files_json      TEXT NOT NULL DEFAULT '[]',
    -- consumes and produces are superpowers' idiom: the agent executing one
    -- task sees only that task's row, never the plan around it or the
    -- sibling tasks before and after. Without these two columns it has no
    -- way to learn the exact names and signatures the prior task promised
    -- it, or that the next task is depending on it to produce.
    consumes        TEXT,
    produces        TEXT,
    -- Both NOT NULL: a task with no way to check it was actually done is not
    -- a task, and the schema refuses to let a plan skip either one rather
    -- than trusting every future plan to remember.
    verify_command  TEXT NOT NULL,
    expected_output TEXT NOT NULL,
    -- Requirement names this task implements. Exists so "which requirement
    -- has no task covering it" is a query against this column, not a
    -- self-grade the model performing the work hands itself.
    --
    -- Names rather than keys, and the name is fixed at the moment of
    -- planning: a later delta may rename the requirement it points at, and
    -- nothing here follows. Whoever writes the coverage query owns that —
    -- matching on the live name alone would report a covered requirement as
    -- uncovered the first time one is renamed.
    covers_json     TEXT NOT NULL DEFAULT '[]',
    status          TEXT NOT NULL CHECK (status IN ('pending', 'running', 'done', 'skipped')),
    -- evidence and verified_at land together: a task is marked done at the
    -- same time as what verify_command printed is captured. A checked box
    -- with no evidence is a claim the next session has no way to audit and
    -- will build on regardless.
    evidence        TEXT,
    verified_at     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

CREATE UNIQUE INDEX tasks_number ON tasks (change_id, number);

-- A `task_reviews` table belongs here eventually: superpowers runs a review
-- cycle whose rounds are worth keeping, and whose cap — rounds 1-3 back to
-- the same implementer, 4-5 to a fresh one on a stronger model, no round 6 —
-- is better expressed as a CHECK than as a convention.
--
-- It is not here yet because nothing in this slice would write to it, and a
-- table with no writer is the mistake this codebase has just finished paying
-- for one layer down: `distill_session` had a producer, no consumer, and a
-- console that counted the backlog as work. The table lands with the code
-- that fills it.
