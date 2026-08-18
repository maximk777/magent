-- The journal a replan cannot delete.
--
-- `Store::plan` begins by deleting the change's tasks, deliberately: a replan
-- replaces the plan rather than appending to it, and a reused number would
-- otherwise fight `tasks_number`. Replanning a change in `planned` status is
-- legal, which is to say mid-execution — so the rows that go can be ones a
-- tick had already written `evidence` and `verified_at` onto, and `tasks`
-- keeps no history. The one thing the design exists to make unforgeable sat in
-- the single most deletable place.
--
-- Making the delete gentler is not the fix: a new plan that renumbers the work
-- would then collide with the survivors and fail exactly where replanning is
-- legitimate. A plan is a statement about what remains to be done; a tick is a
-- record of something that happened, and nothing that happened stops having
-- happened because the plan changed.
CREATE TABLE task_ticks (
    id             TEXT PRIMARY KEY,
    change_id      TEXT NOT NULL REFERENCES sdd_changes(id) ON DELETE CASCADE,
    -- No foreign key to `tasks`, and that is the point rather than an
    -- oversight: a tick records what happened, a task row records what is
    -- planned, and one outliving the other is the whole reason this table
    -- exists. The number is carried as written, as in `tasks.number`, so a
    -- reader can line a tick up against the plan it was taken under.
    number         TEXT NOT NULL,
    verify_command TEXT NOT NULL,
    output         TEXT NOT NULL,
    missing_json   TEXT NOT NULL DEFAULT '[]',
    run_id         TEXT NOT NULL,
    created_at     TEXT NOT NULL
);

-- No UNIQUE on (change_id, number): a task closed twice is two ticks, and that
-- is a fact worth keeping. A journal that kept only the latest would read
-- afterwards exactly like one where the task was proved once.
CREATE INDEX task_ticks_change ON task_ticks (change_id, created_at);
