-- Slice 3: repositories that belong together, and what each one is for.
--
-- A repository is rarely the right unit for everything known. How a group of
-- services authenticate to each other is true of all of them, and a role says
-- how freely each may be touched: deploying infrastructure deserves a different
-- posture from the service being worked on.

ALTER TABLE repositories ADD COLUMN role TEXT NOT NULL DEFAULT 'primary'
    CHECK (role IN ('primary', 'related', 'read_only', 'infrastructure'));

-- A workspace is addressed by name when grouping, so two groups must not end up
-- sharing one and silently merging their memory.
CREATE UNIQUE INDEX workspaces_name ON workspaces(name);
