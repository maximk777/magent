-- Slice 2: durable memory.
--
-- A fact carries where it applies, how it may conflict, how much it is trusted
-- and what it was learned from. Superseding never deletes: knowing what was
-- believed earlier is how a wrong turn gets diagnosed.

CREATE TABLE facts (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    title        TEXT NOT NULL,
    body         TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('user', 'feedback', 'project', 'reference')),
    scope        TEXT NOT NULL CHECK (scope IN ('user', 'workspace', 'repository', 'run')),
    cardinality  TEXT NOT NULL CHECK (cardinality IN ('single', 'set', 'timeline')),
    status       TEXT NOT NULL CHECK (status IN (
        'observed', 'inferred', 'verified', 'contradicted', 'stale', 'revoked'
    )),
    confidence   REAL NOT NULL,

    workspace_id TEXT REFERENCES workspaces(id),
    run_id       TEXT REFERENCES runs(id),
    -- Where imported memory was filed before its workspace was known. Kept even
    -- after binding, because it is the provenance of the import.
    namespace    TEXT,

    -- NULL for the current value; set on a value that has been replaced.
    superseded_by TEXT REFERENCES facts(id),
    valid_from   TEXT,
    valid_to     TEXT,

    source_run_id     TEXT REFERENCES runs(id),
    source_session_id TEXT REFERENCES sessions(id),
    provenance   TEXT NOT NULL DEFAULT 'session',

    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE fact_evidence (
    id           TEXT PRIMARY KEY,
    fact_id      TEXT NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    locator      TEXT NOT NULL,
    excerpt      TEXT,
    created_at   TEXT NOT NULL
);

-- to_name rather than a foreign key: a link to a fact nobody has written yet
-- records something worth knowing, namely that it is missing.
CREATE TABLE fact_relations (
    id           TEXT PRIMARY KEY,
    from_fact_id TEXT NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    to_name      TEXT NOT NULL,
    predicate    TEXT NOT NULL CHECK (predicate IN (
        'related', 'supersedes', 'contradicts', 'refines'
    )),
    created_at   TEXT NOT NULL
);

-- Derived and rebuildable: the facts table is the source of truth.
CREATE VIRTUAL TABLE facts_fts USING fts5(
    name, title, body,
    content = 'facts',
    content_rowid = 'rowid',
    tokenize = "unicode61 remove_diacritics 2"
);

CREATE TRIGGER facts_fts_insert AFTER INSERT ON facts BEGIN
    INSERT INTO facts_fts(rowid, name, title, body)
    VALUES (new.rowid, new.name, new.title, new.body);
END;

CREATE TRIGGER facts_fts_delete AFTER DELETE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, name, title, body)
    VALUES ('delete', old.rowid, old.name, old.title, old.body);
END;

CREATE TRIGGER facts_fts_update AFTER UPDATE ON facts BEGIN
    INSERT INTO facts_fts(facts_fts, rowid, name, title, body)
    VALUES ('delete', old.rowid, old.name, old.title, old.body);
    INSERT INTO facts_fts(rowid, name, title, body)
    VALUES (new.rowid, new.name, new.title, new.body);
END;

CREATE INDEX facts_lookup ON facts(name, namespace, superseded_by);
CREATE INDEX facts_scope ON facts(scope, namespace, status);
CREATE INDEX facts_workspace ON facts(workspace_id, status);
CREATE INDEX fact_relations_from ON fact_relations(from_fact_id);
CREATE INDEX fact_evidence_fact ON fact_evidence(fact_id);
