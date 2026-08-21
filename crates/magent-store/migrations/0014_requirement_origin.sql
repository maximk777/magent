-- Which change last wrote a requirement: the addition that filed it, or the
-- modification or rename that replaced it. A reader meets a requirement while
-- deciding what to propose against it, and what they want is the reasoning
-- behind it; sending them to look for a change by name assumes they already
-- know which one to look for, which is what they are trying to find out.
--
-- Nullable, and rows written before this stay NULL: the link for an `added`
-- delta was never stored (archiving mints a fresh id and does not write it
-- back), so a backfill could only match on capability and name, and a name
-- reused across changes would attribute a requirement to the wrong one. An
-- absence a reader can see beats a guess dressed as provenance.

ALTER TABLE requirements ADD COLUMN origin_change_id TEXT REFERENCES sdd_changes(id);
