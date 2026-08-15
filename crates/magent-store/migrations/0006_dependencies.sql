-- Reference checkouts: repositories a workspace reads but does not work in.
--
-- Only the declaration is canonical. The checkout on disk and everything
-- derived from it can be deleted and rebuilt from these rows, which is why the
-- path is not stored: it is a function of the deps root and the slug, and
-- storing it would let the database disagree with the filesystem.
CREATE TABLE dependencies (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    -- The URL as typed, kept so that what is shown back is what was asked for.
    url          TEXT NOT NULL,
    -- 'github.com/acme/thing', the same normalisation repositories get, so the
    -- SSH and HTTPS forms of one project are one dependency.
    identity_key TEXT NOT NULL,
    -- NULL means whatever the remote's default branch is.
    git_ref      TEXT,
    -- 'github.com/acme/thing@v1.2.0'. Derived from the two columns above and
    -- stored anyway: it is the on-disk name, and recomputing it differently
    -- after a change to normalisation would orphan every existing checkout.
    slug         TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('declared', 'present', 'failed')),
    -- The commit the checkout is at. What makes a reference source citable.
    revision     TEXT,
    synced_at    TEXT,
    last_error   TEXT,
    created_at   TEXT NOT NULL
);

-- One checkout per project per ref. Wanting v1 and v2 side by side is a real
-- thing to want, so the ref is part of the identity; declaring the same pair
-- twice is not.
CREATE UNIQUE INDEX dependencies_identity
    ON dependencies (workspace_id, identity_key, IFNULL(git_ref, ''));
