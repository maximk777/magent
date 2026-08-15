-- Two corrections to slice 3.
--
-- 1. A workspace created implicitly, one per repository, is not addressable by
--    name and must not compete for one. The unique index added in 0004 assumed
--    every workspace was an explicit group, so two projects whose directories
--    happen to share a basename collided and the second could not be resolved
--    at all.
--
-- 2. Nothing schema-level for the identity upgrade, which lives in the resolver;
--    this migration only makes room for it by allowing a repository's
--    identity_key to change in place.

ALTER TABLE workspaces ADD COLUMN explicit INTEGER NOT NULL DEFAULT 0;

-- Anything grouped by hand before this migration was explicit by definition:
-- implicit workspaces are named after a directory, explicit ones after a group.
UPDATE workspaces SET explicit = 1
WHERE id IN (SELECT workspace_id FROM repositories GROUP BY workspace_id HAVING COUNT(*) > 1);

DROP INDEX workspaces_name;

-- Only an explicitly named group owns its name.
CREATE UNIQUE INDEX workspaces_explicit_name ON workspaces(name) WHERE explicit = 1;
