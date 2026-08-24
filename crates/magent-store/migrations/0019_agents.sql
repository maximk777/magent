-- A subagent, by the id the harness gives it.
--
-- Not a session: subagents share their parent's session id by design, which is
-- why sessions.parent_session_id sits unused. What the harness does give is
-- `agent_id`, unique per spawned subagent, in the payload of PostToolUse and
-- SubagentStop.
--
-- The row is written lazily, on the first event naming the agent: nothing tells
-- the store a subagent was spawned, so the first thing it does is the first
-- thing anybody can see. started_at is therefore first-seen, not dispatch.
--
-- id is the first TEXT PRIMARY KEY in this schema whose value is not
-- generated here: sessions.id, runs.id and checkpoints.id are all minted by
-- this store, which is what makes their uniqueness and non-emptiness free.
-- agent_id is taken verbatim from the harness instead, and this table trusts
-- it to be globally unique and non-empty without checking either — that is
-- the writer's obligation, not the schema's.
CREATE TABLE agents (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    agent_type TEXT,
    started_at TEXT NOT NULL,
    ended_at   TEXT
);

-- Task 5's read of a run's agents belongs here: joining agents to sessions on
-- session_id, filtered to sessions.run_id, is what this index is for. It is
-- not here yet because nothing in this slice reads agents by session_id — the
-- index lands ahead of its reader the way 0009's task_reviews note describes,
-- on the strength that the query is coming, not that it exists.
CREATE INDEX agents_session ON agents(session_id);

-- Which agent made an edit. ON DELETE SET NULL for the reason 0018 learned the
-- hard way: the ledger is the history of what was edited, and losing the
-- attribution is honest where losing the row is not.
--
-- agents.session_id above carries no ON DELETE at all, which makes it
-- NO ACTION — the strictest of the two references this migration adds.
-- Nothing deletes a session or its run today, and workspace deletion
-- explicitly excludes workspaces that still have runs, so that default is
-- unreachable; revisit if either of those ever changes.
ALTER TABLE file_ledger ADD COLUMN agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL;
