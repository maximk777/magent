---
name: sdd-execute
description: Use when working through an agreed task list in openspec/changes/<id>/tasks.md. Keeps the run bound to the task in hand so a compaction or a handoff resumes at the right step, and refuses to tick a box whose verification has not been run.
---

# Execute the plan

## Bind the run to the task

```
magent_start        { task: "<change-id>: <task text>" }
magent_checkpoint   { stage: "executing",
                      spec_change_id: "<change-id>",
                      spec_paths: ["openspec/changes/<change-id>/tasks.md"],
                      current_task: "2: wire the budget into the client",
                      handoff_summary: "..." }
```

This is what a plan on disk cannot do by itself. The files say what the work is;
the binding says which step is in hand *right now*, in this session. After a
compaction the restored context then reads `On task: 2: wire the budget` rather
than whichever prompt happened to open the run — the difference between
resuming and starting over.

Set `spec_change_id` and `spec_paths` once. Later checkpoints need only
`current_task`; omitting the others leaves them bound.

## One task at a time

Work the list in order. For each:

1. Say which task you are on.
2. Write the test the task calls for, and watch it fail **for the right
   reason** — not a typo, not a missing import. A test that fails because it
   does not compile proves nothing.
3. Make it pass.
4. Run the task's stated verification. Not a similar command; the stated one.
5. Only then tick the box in `tasks.md`.
6. `magent_checkpoint` with the new `current_task`, and with what you decided
   and what you rejected. File edits are recorded automatically — do not
   restate them.

Never tick a box whose verification you did not run. If you skipped it, say
which one and why, and leave the box unticked. An unticked box is information;
a ticked one that was never checked is a lie the next session will act on.

## When the plan turns out wrong

It will, on any plan worth making. Change `tasks.md` and say what changed —
splitting a task, reordering, dropping one that turned out unnecessary. What
you must not do is quietly build something else: the file is the shared
artifact, and a plan that no longer describes the work is worse than no plan,
because people still trust it.

If the *proposal* turns out wrong — the approach, not the steps — stop and say
so rather than patching the task list around it.

## Long plans

Context degrades over a plan of any length. If a fresh subagent per task is
available, use it: each starts clean, reads `tasks.md` and the checkpoint, does
one task, and reports. The binding is what makes that work — a new context can
find its place from the run alone.

## Finishing

When the last box is ticked and its verification has run:

```
magent_finish { action: "complete_run", outcome: "<what is now true>" }
```

Record with `magent_remember` anything durable the work taught: a constraint
found the hard way, a command that turned out to be the real check here.
