---
name: sdd-execute
description: Use when working through an agreed task list in Magent's store. Runs tasks one at a time — inline or by dispatching a fresh subagent per task with two-stage review — keeps the Magent run bound to the task in hand so a compaction resumes at the right step, and closes a task only with the command the plan named and what that command printed.
argument-hint: "[change-slug]"
allowed-tools: Bash(magent changes:*)
---

# Execute the plan

**Change:** $ARGUMENTS

Where things stand:

!`magent changes 2>&1 || echo "magent is not installed: run ./scripts/install.sh in the Magent checkout"`

Before the first task, call `magent_status`. If a run is already bound to a
change, that is what is being resumed — pick up at its `current_task` rather
than starting the list again.

Call `magent_changes` naming the change, extract every task with its full text,
and hold them. Then work them in order.

## Bind the run to the task

```
magent_start        { task: "<slug>: <task number and title>" }
magent_checkpoint   { stage: "executing",
                      spec_change_id: "<slug>",
                      current_task: "1.2 Implement RetryBudget",
                      handoff_summary: "..." }
```

This is what neither the plan nor a subagent can supply. The plan says what the
work is; the binding says which step is in hand **right now**, in this session.
After a compaction the restored context reads `On task: 1.2 Implement
RetryBudget` rather than whichever prompt opened the run — the difference
between resuming and starting over.

Set `spec_change_id` once. Later checkpoints need only `current_task`; omitting
the rest leaves them bound.

## Per task, whoever does it

1. Say which task you are on.
2. Write the test the task calls for. Run it. Confirm it fails **for the right
   reason** — not a typo, not a missing import. A test that fails to compile
   proves nothing.
3. Make it pass with the minimal change.
4. Run the task's stated verification. Not a similar command; the stated one.
5. Close the task with what that run proved:

   ```
   magent_checkpoint { stage: "executing",
                       spec_change_id: "<slug>",
                       current_task: "1.3 wire the budget",
                       task_done: { number: "1.3",
                                    verify_command: "cargo test budget",
                                    output: "<what it printed>" } }
   ```

6. Commit.
7. `magent_checkpoint` with the next `current_task`, plus what you decided and
   what you rejected. File edits are captured automatically — do not restate
   them.

A task closes on the command the plan recorded and nothing else: a
`verify_command` that differs is refused, and the refusal names the one the plan
recorded, so there is no closing a task by quietly checking something easier.
What the store cannot see is whether you ran it, and that is what `output` is
for — paste what the run printed. An open task is information; a closed one
whose evidence was invented is a lie the next session acts on. If you genuinely
skipped a check, leave the task open and say which and why.

## Subagent-driven execution

Preferred for any plan longer than a few tasks: context degrades over a long
plan, and a fresh subagent per task does not inherit the degradation.

Dispatch one implementer per task — never two in parallel, they collide — then
review in two stages before moving on.

**Give the subagent the task's full text.** Do not tell it to look the task up.
You have the text already; making it fetch the plan costs a tool call and lets
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

It will, on any plan worth making. Call `magent_plan` again with the whole
corrected list — split, reorder, drop — and say what changed and why. What you
must not do is quietly build something else: the plan is the shared artifact,
and one that no longer describes the work is worse than no plan, because people
still trust it.

Re-planning replaces the tasks rather than amending them: the tasks already
closed have to be resent, they come back open, and the evidence they carried is
gone. So correct a plan once, deliberately, with the whole list in front of you
— not task by task as each one turns out slightly off.

If the **proposal** is wrong — the approach, not the steps — stop and say so
rather than patching tasks around it.

## Finishing

Closing the last open task is what moves the change to `ready`. Then dispatch
one final review over the whole change, and:

```
magent_archive { change: "<slug>" }
```

Archiving is the step that makes this worth doing: the change's added, modified
and removed requirements fold into the live specification, which becomes what is
currently true for the next change, while the change itself is kept with its
reasoning intact. It refuses while any task is still open, and lists the ones
that are — so a refusal here is a task you thought was done and never closed.

```
magent_finish { action: "complete_run", outcome: "<what is now true>" }
```

Record with `magent_remember` anything durable the work taught: a constraint
found the hard way, a command that turned out to be the real check here.
