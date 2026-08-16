-- SDD: the spec-driven process, as rows instead of openspec/ markdown.
--
-- The graph is OpenSpec's: a change carries artifacts and proposes deltas
-- against capabilities, whose requirements each need at least one scenario.
-- What moves here is the validation a markdown parser had to do by hand:
-- a scenario is a row with a foreign key, not three or four hashtags that
-- fail silently if miscounted.

CREATE TABLE sdd_changes (
    id             TEXT PRIMARY KEY,
    workspace_id   TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    namespace      TEXT,
    slug           TEXT NOT NULL,
    title          TEXT NOT NULL,
    classification TEXT NOT NULL CHECK (classification IN ('spike', 'bounded', 'architectural')),
    skip_specs     INTEGER NOT NULL DEFAULT 0,
    status         TEXT NOT NULL CHECK (status IN ('drafting', 'specified', 'planned', 'executing', 'ready', 'archived', 'abandoned')),
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

-- A name is taken only by a change still in flight: archiving or abandoning
-- one frees its slug for reuse, the way a merged branch frees its name.
CREATE UNIQUE INDEX sdd_changes_live_slug
    ON sdd_changes (workspace_id, IFNULL(namespace, ''), slug)
    WHERE status NOT IN ('archived', 'abandoned');

-- One row per stage of the process. body_json rather than a column per kind,
-- the idiom checkpoints.payload_json already uses: what a proposal or a task
-- list looks like is a Rust concern, not a schema one.
CREATE TABLE sdd_artifacts (
    id         TEXT PRIMARY KEY,
    change_id  TEXT NOT NULL REFERENCES sdd_changes(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL CHECK (kind IN ('proposal', 'design', 'specs', 'tasks')),
    body_json  TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX sdd_artifacts_kind ON sdd_artifacts (change_id, kind);

-- The shipped surface of the product, independent of any change proposing
-- work against it. Archiving a change folds its deltas in here.
CREATE TABLE capabilities (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    namespace    TEXT,
    path         TEXT NOT NULL,
    purpose      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE UNIQUE INDEX capabilities_path
    ON capabilities (workspace_id, IFNULL(namespace, ''), path);

-- 'removed' rather than deleted: a requirement a change retired is still
-- part of the capability's history, the way a REMOVED delta is kept legible
-- instead of erased.
CREATE TABLE requirements (
    id            TEXT PRIMARY KEY,
    capability_id TEXT NOT NULL REFERENCES capabilities(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    text          TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('live', 'removed')),
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE UNIQUE INDEX requirements_name
    ON requirements (capability_id, name) WHERE status = 'live';

-- 'when' and 'then' are reserved words, hence the suffix; 'given' is not, but
-- the third column keeps the Gherkin triple visibly matched.
CREATE TABLE scenarios (
    id             TEXT PRIMARY KEY,
    requirement_id TEXT NOT NULL REFERENCES requirements(id) ON DELETE CASCADE,
    seq            INTEGER NOT NULL,
    name           TEXT NOT NULL,
    given_text     TEXT,
    when_text      TEXT NOT NULL,
    then_text      TEXT NOT NULL
);

CREATE UNIQUE INDEX scenarios_seq ON scenarios (requirement_id, seq);

-- A change's proposed edit to one requirement. capability_id is nullable
-- because an ADDED delta can name a capability that does not exist yet; the
-- row it points at is only created when the change is archived.
CREATE TABLE spec_deltas (
    id              TEXT PRIMARY KEY,
    change_id       TEXT NOT NULL REFERENCES sdd_changes(id) ON DELETE CASCADE,
    capability_path TEXT NOT NULL,
    capability_id   TEXT REFERENCES capabilities(id),
    purpose         TEXT,
    op              TEXT NOT NULL CHECK (op IN ('added', 'modified', 'removed', 'renamed')),
    requirement_id  TEXT REFERENCES requirements(id),
    name            TEXT NOT NULL,
    text            TEXT,
    rename_to       TEXT,
    reason          TEXT,
    migration       TEXT,
    created_at      TEXT NOT NULL
);

CREATE UNIQUE INDEX spec_deltas_identity
    ON spec_deltas (change_id, capability_path, name);

CREATE TABLE delta_scenarios (
    id         TEXT PRIMARY KEY,
    delta_id   TEXT NOT NULL REFERENCES spec_deltas(id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL,
    name       TEXT NOT NULL,
    given_text TEXT,
    when_text  TEXT NOT NULL,
    then_text  TEXT NOT NULL
);

CREATE UNIQUE INDEX delta_scenarios_seq ON delta_scenarios (delta_id, seq);
