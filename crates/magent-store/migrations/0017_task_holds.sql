-- Who is working a task, and until when their hold lasts.
--
-- Nothing wrote `tasks.status = 'running'` before this, though the CHECK in
-- 0009 has allowed it from the start. A hold is what that status means.
--
-- Additive: a server already running against this profile keeps working while
-- it lands, the way 0015 did.
ALTER TABLE tasks ADD COLUMN claimed_by TEXT REFERENCES sessions(id);
ALTER TABLE tasks ADD COLUMN lease_until TEXT;

CREATE INDEX tasks_change_lease ON tasks(change_id, lease_until);
