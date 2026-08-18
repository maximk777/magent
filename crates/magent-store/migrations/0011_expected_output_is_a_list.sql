-- The column held one line of prose a plan wrote before the work, and the
-- tick looked for that whole line inside what the command printed. It never
-- found it: every task of the last change closed with expected_output_found
-- false, including the ones whose commands printed exactly what was expected.
-- A list of short markers is what a plan can actually state ahead of time and
-- what an output can actually be checked against.
--
-- The old value survives as a single-element list rather than being split or
-- discarded: that string is exactly what the plan claimed to be looking for,
-- and carrying it across keeps the row honest rather than inventing markers
-- nobody wrote.
UPDATE tasks SET expected_output = json_array(expected_output);

ALTER TABLE tasks RENAME COLUMN expected_output TO expected_output_json;
