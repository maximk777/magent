---
name: sdd-execute
description: Use when working through an agreed OpenSpec task list. Runs tasks one at a time — inline or by dispatching a fresh subagent per task with two-stage review — keeps the Magent run bound to the task in hand so a compaction resumes at the right step, and refuses to tick a box whose verification has not run.
---

# Execute the plan

Read `openspec/changes/<change-id>/tasks.md` once, extract every task with its
full text, and hold them. Then work them in order.

## Bind the run to the task

```
magent_start        { task: "<change-id>: <task text>" }
magent_checkpoint   { stage: "executing",
                      spec_change_id: "<change-id>",
                      spec_paths: ["openspec/changes/<change-id>/tasks.md"],
                      current_task: "1.2 Implement RetryBudget",
                      handoff_summary: "..." }
```

This is what neither the plan on disk nor a subagent can supply. The files say
what the work is; the binding says which step is in hand **right now**, in this
session. After a compaction the restored context reads `On task: 1.2 Implement
RetryBudget` rather than whichever prompt opened the run — the difference
between resuming and starting over.

Set `spec_change_id` and `spec_paths` once. Later checkpoints need only
`current_task`; omitting the rest leaves them bound.

## Per task, whoever does it

1. Say which task you are on.
2. Write the test the task calls for. Run it. Confirm it fails **for the right
   reason** — not a typo, not a missing import. A test that fails to compile
   proves nothing.
3. Make it pass with the minimal change.
4. Run the task's stated verification. Not a similar command; the stated one.
5. Tick the box in `tasks.md`.
6. Commit.
7. `magent_checkpoint` with the new `current_task`, plus what you decided and
   what you rejected. File edits are captured automatically — do not restate
   them.

Never tick a box whose verification you did not run. An unticked box is
information; a ticked one that was never checked is a lie the next session acts
on. If you skipped a check, say which and why, and leave it unticked.

## Subagent-driven execution

Preferred for any plan longer than a few tasks: context degrades over a long
plan, and a fresh subagent per task does not inherit the degradation.

Dispatch one implementer per task — never two in parallel, they collide — then
review in two stages before moving on.

**Give the subagent the task's full text.** Do not tell it to read the plan
file. You have the text already; making it re-read costs a tool call and lets
it read the wrong task. Include scene-setting: where this fits, what came
before, what it may assume.

Three agents ship with this plugin:

- `sdd-implementer` — does the task, TDD, commits, self-reviews
- `sdd-spec-reviewer` — verifies the code matches the requirement, independently
- `sdd-code-reviewer` — verifies it is well built

Order matters: spec compliance first, then quality. Reviewing the craft of
something that builds the wrong thing wastes both passes.

Loop each review: reviewer finds issues, the same implementer fixes them, the
reviewer looks again. Do not skip the re-review, and do not move on with either
review open.

**Do not stop between tasks to ask whether to continue.** They asked for the
plan to be run. Stop only for a blocker you cannot resolve, or when it is done.

### Model selection

Use the least capable model that can do the job.

- One or two files with a complete spec → a fast, cheap model
- Several files with integration concerns → a standard model
- Design judgement or broad codebase understanding → the most capable

### Status protocol

Implementers report one of four. Handle each:

- **DONE** — proceed to spec review.
- **DONE_WITH_CONCERNS** — read the concerns first. Correctness or scope: fix
  before reviewing. Observations ("this file is getting large"): note and
  proceed.
- **NEEDS_CONTEXT** — supply what was missing and re-dispatch.
- **BLOCKED** — assess. Missing context: provide it. Needs more reasoning:
  re-dispatch on a more capable model. Too large: split it. The plan itself
  wrong: escalate to the human.

Never re-dispatch the same model on the same prompt after a BLOCKED. If it said
it was stuck, something has to change.

## When the plan turns out wrong

It will, on any plan worth making. Edit `tasks.md` and say what changed —
split, reorder, drop. What you must not do is quietly build something else: the
file is the shared artifact, and a plan that no longer describes the work is
worse than no plan, because people still trust it.

If the **proposal** is wrong — the approach, not the steps — stop and say so
rather than patching tasks around it.

## Finishing

When the last box is ticked and its verification has run, dispatch one final
review over the whole change, then:

```bash
openspec validate <change-id>
openspec archive <change-id>
```

Archiving is the step that makes this worth doing: the change's ADDED,
MODIFIED and REMOVED requirements merge into `openspec/specs/`, which becomes
what is currently true for the next change, and the change folder is preserved
with the reasoning intact.

```
magent_finish { action: "complete_run", outcome: "<what is now true>" }
```

Record with `magent_remember` anything durable the work taught: a constraint
found the hard way, a command that turned out to be the real check here.
