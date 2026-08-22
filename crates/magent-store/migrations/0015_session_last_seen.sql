-- When a session was last heard from, so a session whose process died stops
-- outranking one that is still working.
--
-- Additive on purpose. A migration that drops or renames breaks servers already
-- running against this profile; ADD COLUMN passes unnoticed, so this can land
-- while sessions are live.
ALTER TABLE sessions ADD COLUMN last_seen_at TEXT;

-- Existing rows have never been stamped. Their start is the only moment they
-- are known to have been alive, and it keeps the ordering total.
UPDATE sessions SET last_seen_at = started_at WHERE last_seen_at IS NULL;

CREATE INDEX sessions_run_last_seen ON sessions(run_id, last_seen_at DESC);
