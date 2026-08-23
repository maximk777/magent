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
- An `expected_output` written as a sentence about the output rather than a
  fragment of it

## Check before you write the prose

Send the plan twice.

**First with `check_only: true` and no `body` on any task.** The gates read
`covers`, `consumes` and `produces` and nothing else, so a skeleton — numbers,
titles, those three lists, `verify_command` and `expected_output` — answers the
same question. Nothing is written: a change that had a plan still has exactly
that plan, and one that had none still has none. The report says so, so a check
cannot be mistaken for a plan that was stored.

**Then, once it passes, write the bodies and send it for real.**

The reason is the cost of being wrong. A plan is accepted only whole, so a
`covers` list that misses one requirement is refused — correctly — and fixing it
means writing every paragraph in the plan again. One session paid forty-four
kilobytes for that: thirty-three tasks, the largest request this profile has
ever recorded, refused for twenty-five uncovered requirements, and from outside
it read as a hang. The skeleton for the same plan is under seven kilobytes.

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
                  produces: ["the test budget::caps_attempts, failing"],
                  verify_command: "cargo test budget::caps_attempts",
                  expected_output: ["no function or associated item named `new`",
                                    "RetryBudget"],
                  covers: ["A retry budget caps attempts"] },
                { number: "1.2",
                  title: "Implement RetryBudget in src/budget.rs",
                  body: "RetryBudget::new(max: u32) and take(&mut self) -> bool,
                         counting down and returning false once the count is
                         spent. No clock and no jitter yet; 2.1 adds those.",
                  files: ["src/budget.rs", "src/lib.rs"],
                  consumes: ["the test budget::caps_attempts, failing"],
                  produces: ["RetryBudget::new(max: u32)",
                             "RetryBudget::take(&mut self) -> bool"],
                  verify_command: "cargo test budget",
                  expected_output: ["test result: ok", "caps_attempts ... ok"],
                  covers: ["A retry budget caps attempts"] } ] }
```

`number`, `title`, `verify_command` and `expected_output` are required on every
task; `body`, `files`, `consumes`, `produces` and `covers` are what make it
executable by someone who cannot see the rest of the plan.

`consumes` and `produces` are lists of artifact names, and one entry is one
exact name. The identical string appears in the producing task's `produces` and
the consuming task's `consumes` — look at 1.1 and 1.2 above, where the failing
test is named the same way twice. That repetition is the point rather than
duplication: the agent doing 1.2 never sees 1.1, so a name written differently
is a name it cannot find, and exact equality is what lets the store match them
at all.

Two refusals follow from it. A `consumes` entry no task in the plan `produces`
is refused and named, because an agent told to build on something nobody makes
will guess, and guess plausibly. A plan whose tasks wait on each other is
refused with the tasks in the cycle named, because no task in it can ever be
first — including a task that consumes what it produces itself.

What this buys is the reason to be exact: from those edges the store computes
what may be started now and how wide the plan could ever run, so whether two
tasks can go side by side stops being a judgement and becomes a query.

`expected_output` is one or more markers, and a marker is a string the command
will print verbatim — the invariant fragments of a line rather than the line
itself. Counts that shift between runs, and prose the plan invented, belong in
the task's `body`: the tick names every marker it did not find, so a marker the
command never prints is reported missing on every run and turns that report
into noise. Sixteen tasks of this project's own work closed that way while the
process was being built, their expected output reported missing every time —
including the tasks whose command had printed precisely what was expected.

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
