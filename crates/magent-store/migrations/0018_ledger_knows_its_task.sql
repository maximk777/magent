-- Which task an edit was made for, and whose file it landed on.
--
-- Both are stamped when the row is written, because neither can be recovered
-- afterwards: a session works task 5, then 6, then 7 in one run, and 0017 keeps
-- only who holds a task now and until when. A question asked later about a
-- moment already past has no answer at all.
--
-- task_id is a key and trespass_on is a number, which is not an oversight. The
-- number is the address every message and every plan uses. The CHECK below
-- only ties the pair together — trespass_on cannot be set without a task_id
-- to resolve it against — it does not defend the number itself: a replan
-- renumbers a change's tasks, and nothing here follows that renumbering.
--
-- task_id is ON DELETE SET NULL rather than CASCADE or the default NO ACTION.
-- Foreign keys are on in this store, and a replan deletes and reinserts a
-- change's tasks (`DELETE FROM tasks WHERE change_id = ?1`), which NO ACTION
-- would turn into a failed replan the first time an edit had been recorded
-- against one of those tasks. The ledger is the history of what was edited,
-- and CASCADE would destroy that history because a plan was rewritten;
-- losing only the attribution is the honest outcome, and it leaves the row
-- reading the same as it does when nobody held a task.
--
-- Additive, the way 0015 and 0017 were: a server already running against this
-- profile keeps working while it lands.
ALTER TABLE file_ledger ADD COLUMN task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL;
ALTER TABLE file_ledger ADD COLUMN trespass_on TEXT
    CHECK (trespass_on IS NULL OR task_id IS NOT NULL);

-- A future close_task belongs here: reading the ledger by the task it
-- belongs to, inside the checkpoint's transaction, is what this index is
-- for. It is not here yet because nothing in this slice reads by task_id —
-- the index lands ahead of its reader the way 0009's task_reviews note
-- describes, on the strength that the query is coming, not that it exists.
CREATE INDEX file_ledger_task ON file_ledger(task_id);
