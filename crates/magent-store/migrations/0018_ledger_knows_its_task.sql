-- Which task an edit was made for, and whose file it landed on.
--
-- Both are stamped when the row is written, because neither can be recovered
-- afterwards: a session works task 5, then 6, then 7 in one run, and 0017 keeps
-- only who holds a task now and until when. A question asked later about a
-- moment already past has no answer at all.
--
-- task_id is a key and trespass_on is a number, which is not an oversight. The
-- number is the address every message and every plan uses, and the row already
-- fixes the change through task_id, so the pair identifies the other task
-- exactly while reading as what a person would be told.
--
-- Additive, the way 0015 and 0017 were: a server already running against this
-- profile keeps working while it lands.
ALTER TABLE file_ledger ADD COLUMN task_id TEXT REFERENCES tasks(id);
ALTER TABLE file_ledger ADD COLUMN trespass_on TEXT;

-- Closing a task reads the ledger by the task it belongs to, and that read
-- happens inside the checkpoint's transaction.
CREATE INDEX file_ledger_task ON file_ledger(task_id);
