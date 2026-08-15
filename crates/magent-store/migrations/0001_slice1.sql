-- Slice 1: run identity, sessions, checkpoints, the file ledger, idempotency
-- and the background job queue.
--
-- Facts, evidence, revisions and dependency indexing arrive in later slices;
-- the columns they need are not stubbed here on purpose.

CREATE TABLE schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE workspaces (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE repositories (
    id             TEXT PRIMARY KEY,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id),
    -- 'git:<normalised origin>' when the repository has one, else
    -- 'path:<canonical root>'. Origin-first so that clones, subdirectories and
    -- linked worktrees of one project share a single identity and therefore a
    -- single memory.
    identity_key   TEXT NOT NULL UNIQUE,
    -- The root this repository was first seen at. Informational: a worktree of
    -- the same project resolves here under a different path.
    canonical_root TEXT NOT NULL,
    origin_url     TEXT,
    created_at     TEXT NOT NULL
);

CREATE TABLE runs (
    id             TEXT PRIMARY KEY,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id),
    task           TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('open', 'completed')),
    stage          TEXT NOT NULL,
    outcome        TEXT,
    -- References into openspec/; the files stay the source of truth.
    spec_change_id TEXT,
    spec_paths     TEXT,
    current_task   TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

CREATE TABLE sessions (
    id                    TEXT PRIMARY KEY,
    run_id                TEXT NOT NULL REFERENCES runs(id),
    harness               TEXT NOT NULL,
    external_session_hint TEXT,
    -- Set when a subagent session is rolled up into its parent run.
    parent_session_id     TEXT REFERENCES sessions(id),
    started_at            TEXT NOT NULL,
    ended_at              TEXT
);

CREATE TABLE checkpoints (
    id           TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL REFERENCES runs(id),
    session_id   TEXT NOT NULL REFERENCES sessions(id),
    operation_id TEXT NOT NULL UNIQUE,
    stage        TEXT NOT NULL,
    origin       TEXT NOT NULL CHECK (origin IN ('deterministic', 'enriched')),
    payload_json TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE TABLE file_ledger (
    id          INTEGER PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    session_id  TEXT NOT NULL REFERENCES sessions(id),
    path        TEXT NOT NULL,
    tool        TEXT NOT NULL,
    observed_at TEXT NOT NULL
);

-- Replay protection for every mutation. The request is kept so a reused id
-- carrying a different body can be rejected instead of silently answered.
CREATE TABLE operations (
    operation_id   TEXT PRIMARY KEY,
    operation_kind TEXT NOT NULL,
    request_json   TEXT NOT NULL,
    response_json  TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

CREATE TABLE jobs (
    kind            TEXT NOT NULL,
    job_key         TEXT NOT NULL,
    status          TEXT NOT NULL CHECK (status IN ('pending', 'running', 'done', 'failed')),
    payload_json    TEXT NOT NULL,
    worker_id       TEXT,
    lease_until     TEXT,
    retry_at        TEXT,
    retry_remaining INTEGER NOT NULL DEFAULT 3,
    last_error      TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    PRIMARY KEY (kind, job_key)
);

CREATE INDEX checkpoints_run_rowid ON checkpoints(run_id, id);
CREATE INDEX sessions_run_started ON sessions(run_id, started_at DESC);
CREATE INDEX file_ledger_run ON file_ledger(run_id, id DESC);
CREATE INDEX runs_workspace_status ON runs(workspace_id, status, updated_at DESC);
CREATE INDEX jobs_kind_status ON jobs(kind, status, retry_at, lease_until);
