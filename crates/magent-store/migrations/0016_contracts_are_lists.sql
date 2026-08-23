-- The columns held free prose, so "which task waits for which" was a sentence a
-- reader had to interpret rather than a graph anything could compute. As lists
-- of artifact names they can be matched exactly, and a consumes entry nothing
-- produces becomes a refusal instead of an executing agent's guess.
--
-- The old value survives as a single-element list rather than being split on
-- punctuation: that string is exactly what the plan claimed, and carrying it
-- across keeps the row honest rather than inventing entries nobody wrote. It
-- will match nothing, which is harmless -- the rule is enforced where a plan is
-- written, not against rows already stored.
UPDATE tasks SET consumes = json_array(consumes) WHERE consumes IS NOT NULL;
UPDATE tasks SET consumes = '[]' WHERE consumes IS NULL;
UPDATE tasks SET produces = json_array(produces) WHERE produces IS NOT NULL;
UPDATE tasks SET produces = '[]' WHERE produces IS NULL;

ALTER TABLE tasks RENAME COLUMN consumes TO consumes_json;
ALTER TABLE tasks RENAME COLUMN produces TO produces_json;
