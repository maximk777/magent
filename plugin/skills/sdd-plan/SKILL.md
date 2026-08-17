---
name: sdd-plan
description: Use when a proposal is agreed and before touching code, to turn it into the change's task list. Write it for an engineer who is skilled but knows nothing about this codebase — every step names its files, its command, and its expected output.
argument-hint: "[change-slug]"
allowed-tools: Bash(magent changes:*)
---

# Plan the work

**Change:** $ARGUMENTS

Changes available:

!`magent changes 2>&1 || echo "magent is not installed: run ./scripts/install.sh in the Magent checkout"`

If no change was named and exactly one is open, that is the one. If several
are, ask which — planning the wrong one is an hour nobody gets back.

The output is one `magent_plan` call.

**Announce at the start:** "I'm using sdd-plan to write the implementation
plan."

Read the proposal and its requirement deltas first: `magent_changes` naming the
change returns both. If there is no proposal, the honest move is
`sdd-brainstorm` — planning something nobody agreed to is work that gets thrown
away.

## Who you are writing for

Assume a skilled developer who knows nothing about this codebase or its
domain, and who does not know good test design. Everything they need goes in
the plan: exact paths, the actual code, the exact command, the expected output.

## File structure first

Before any task, map which files are created or modified and what each is
responsible for. This is where the decomposition gets locked in.

Each file gets one clear responsibility and a well-defined interface. Files
that change together live together — split by responsibility, not by technical
layer. In an existing codebase, follow the established patterns; a file you are
already modifying that has grown unwieldy may be split, but do not restructure
things outside the change.

## Step granularity

Each step is **one action, two to five minutes**:

- Write the failing test
- Run it and confirm it fails **for the right reason**
- Write the minimal code to pass
- Run it and confirm it passes
- Commit

A task is a handful of those. If a task's check cannot run until three tasks
later, it is not a task.

## No placeholders

These are plan failures. Never write them:

- "TBD", "TODO", "implement later", "fill in details"
- "Add appropriate error handling", "handle edge cases", "add validation"
- "Write tests for the above" without the actual test code
- "Similar to Task N" — repeat it; tasks get read out of order
- A step that says what to do without showing how
- A reference to a type or function no task defines

## The call

The whole list goes in one `magent_plan`. A second call **replaces** the tasks
rather than adding to them, so a revision resends the plan entire — including
the tasks that were already fine.

```
magent_plan { change: "add-retry-budget",
              tasks: [
                { number: "1.1",
                  title: "Write the failing test for the attempt cap",
                  body: "Add `caps_attempts` to tests/budget.rs: build a
                         RetryBudget of 3, spend all three, assert the fourth
                         attempt is refused. It must fail because RetryBudget
                         has no constructor yet — a failure naming anything
                         else in the test file is a bug in the test.",
                  files: ["tests/budget.rs"],
                  produces: "the test budget::caps_attempts, failing",
                  verify_command: "cargo test budget::caps_attempts",
                  expected_output: "no function or associated item named `new` found for struct `RetryBudget`",
                  covers: ["A retry budget caps attempts"] },
                { number: "1.2",
                  title: "Implement RetryBudget in src/budget.rs",
                  body: "RetryBudget::new(max: u32) and take(&mut self) -> bool,
                         counting down and returning false once the count is
                         spent. No clock and no jitter yet; 2.1 adds those.",
                  files: ["src/budget.rs", "src/lib.rs"],
                  consumes: "the test budget::caps_attempts from 1.1",
                  produces: "RetryBudget::new(u32), RetryBudget::take(&mut self) -> bool",
                  verify_command: "cargo test budget",
                  expected_output: "test result: ok. 1 passed; 0 failed",
                  covers: ["A retry budget caps attempts"] } ] }
```

`number`, `title`, `verify_command` and `expected_output` are required on every
task; `body`, `files`, `consumes`, `produces` and `covers` are what make it
executable by someone who cannot see the rest of the plan. `consumes` and
`produces` repeat the exact names and signatures across the seam, because the
agent doing 1.2 never sees 1.1.

The numbering matters beyond tidiness: a run binds to its task by that number,
and the checkpoint that closes the task names it. Numbers are addresses, not
positions.

## Self-review against the spec

Run this yourself; it is a checklist, not a subagent dispatch.

1. **Coverage** — the store already refuses a plan that leaves a requirement
   the change proposes out of every task's `covers`, so the question here is
   not whether coverage exists but whether it is meaningful. Walk each
   requirement, read the task claiming it, and ask whether that task actually
   makes the requirement true. A `covers` entry added to satisfy the check is
   the one failure the store cannot see.
2. **Placeholders** — scan for every pattern above.
3. **Name consistency** — does a function called `clear_layers` in task 3 stay
   `clear_layers` in task 7? A rename between tasks is a bug in the plan.

Fix inline and move on.

## Hand-off

Show the list and let it be corrected before anything is built. A plan is cheap
to change and expensive to have been wrong about.

Then offer the choice:

> Plan recorded for `<slug>`. Two ways to run it:
>
> **1. Subagent-driven (recommended)** — a fresh subagent per task, two-stage
> review between tasks, no waiting on me between steps.
>
> **2. Inline** — I work the tasks in this session, checkpointing at each.
>
> Which?

Either way the next skill is `sdd-execute`.
