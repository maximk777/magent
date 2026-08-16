---
name: sdd-plan
description: Use after a proposal is agreed and before writing code, to turn it into an ordered task list. Each task carries its own verification, because a task with no way to check it can only be abandoned, never finished.
---

# Plan the work

The output is `openspec/changes/<change-id>/tasks.md`, beside the proposal.

Read the proposal first. If there is not one, the honest move is `sdd-brainstorm`
rather than planning something nobody agreed to.

## What makes a task

Each one states how it will be checked. This is the whole discipline: a task
without a verification cannot be finished, only abandoned, and a plan of those
produces a change that everyone believes is done and nobody has tested.

- **Independently verifiable.** Running its check answers yes or no.
- **Small.** Minutes, not hours. If its check cannot run until three tasks
  later, it is not one task.
- **Leaves the tree working.** It should be possible to commit after each one.
  A plan where only the last task compiles is one long task wearing a costume.
- **Test first, in the same task.** "Write the tests" as a final task is the
  failure mode: by then the code is written and the tests are shaped to it. The
  test belongs in the task that makes it pass.

## What to leave out

Do not plan past what is known. Five tasks you understand beat fifteen where
the last ten are guesses — those get rewritten anyway, and meanwhile they read
as commitments.

Do not include "update the docs" or "add tests" as trailing tasks. Both belong
inside the tasks that created the need.

## The file

```markdown
# <change-id> — tasks

- [ ] 1. <what changes>
      Verify: <the command, and what its output must show>
- [ ] 2. <what changes>
      Verify: <...>
```

Numbered, because `sdd-execute` binds the run to a task by its number and text,
and unnumbered lists renumber themselves the moment one is inserted.

## Then hand it over

Show the list and let it be corrected before any of it is built. A plan is
cheap to change and expensive to have been wrong about.
