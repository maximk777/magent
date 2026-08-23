-- Which task an edit was made for, and whose file it landed on.
--
-- Both are stamped when the row is written, because neither can be recovered
-- afterwards: a session works task 5, then 6, then 7 in one run, and 0017 keeps
-- only who holds a task now and until when. A question asked later about a
-- moment already past has no answer at all.
--
-- task_id is a key and trespass_on is a number, which is not an oversight.
-- The number is the address every message and every plan uses, and a number
-- is not defended against a replan renumbering the tasks. There is no CHECK
-- tying the two together, and that is deliberate, not an omission: the pair
-- is meaningful at the moment it is written, because the writer sets both
-- together — a trespass is resolved only when the editing session holds a
-- task, so task_id is always set alongside it at write time. But
-- ON DELETE SET NULL (below) means a replan can later remove that task,
-- leaving task_id NULL while trespass_on still stands. That row is not
-- corruption: the edit really did land on somebody else's file, and that
-- stays true even after the task it was made for is gone — a row nobody
-- joins to any more, kept because the edit happened. A CHECK that forbade
-- that state would forbid the replan instead, which is the mistake this
-- migration made on its first attempt.
--
-- task_id is ON DELETE SET NULL rather than CASCADE or the default NO ACTION,
-- so a replan's `DELETE FROM tasks WHERE change_id = ?1` clears the
-- attribution instead of failing on it or deleting the row it belongs to.
--
-- Additive, the way 0015 and 0017 were: a server already running against this
-- profile keeps working while it lands.
ALTER TABLE file_ledger ADD COLUMN task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL;
ALTER TABLE file_ledger ADD COLUMN trespass_on TEXT;

-- A future close_task belongs here: reading the ledger by the task it
-- belongs to, inside the checkpoint's transaction, is what this index is
-- for. It is not here yet because nothing in this slice reads by task_id —
-- the index lands ahead of its reader the way 0009's task_reviews note
-- describes, on the strength that the query is coming, not that it exists.
CREATE INDEX file_ledger_task ON file_ledger(task_id);

-- `append_ledger` resolving the editing session's hold, on the hot path:
-- every edit looks up the newest live lease by `claimed_by`, and the only
-- index touching `lease_until` (`tasks_change_lease`, 0017) leads with
-- `change_id`, not `claimed_by`, so that query would otherwise scan and sort
-- the whole table. Partial, because most rows have `claimed_by IS NULL`.
CREATE INDEX tasks_claimed_lease ON tasks(claimed_by, lease_until) WHERE claimed_by IS NOT NULL;
