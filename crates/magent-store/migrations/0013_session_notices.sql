-- What a session has already been told, so it is told once.
--
-- Keyed by the harness's own session string rather than by sessions.id, for the
-- reason retrieval_events gives: the hook has the hint in hand, and resolving it
-- to a row would be a second query on the hot path of every prompt.
--
-- General rather than a boolean column on sessions, because more than one thing
-- will come through this door -- the notes mechanism the console needs delivers
-- its own kind of notice -- and a column per notice means a migration per
-- notice.

CREATE TABLE session_notices (
    id                    INTEGER PRIMARY KEY,
    external_session_hint TEXT NOT NULL,
    kind                  TEXT NOT NULL,
    sent_at               TEXT NOT NULL,
    UNIQUE (external_session_hint, kind)
);
