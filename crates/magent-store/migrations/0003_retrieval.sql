-- Slice 2: what has already been pushed into a session's context.
--
-- The memory index fires on every prompt. Without this, a long session would
-- pay for the same handful of facts on every turn, which is exactly the kind of
-- silent tax that makes a tool get switched off.

CREATE TABLE retrieval_events (
    id                    INTEGER PRIMARY KEY,
    external_session_hint TEXT NOT NULL,
    fact_id               TEXT NOT NULL REFERENCES facts(id) ON DELETE CASCADE,
    pushed_at             TEXT NOT NULL,
    UNIQUE (external_session_hint, fact_id)
);

CREATE INDEX retrieval_events_session ON retrieval_events(external_session_hint);
