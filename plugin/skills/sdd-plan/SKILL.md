---
name: sdd-plan
description: Use when a proposal is agreed and before touching code, to turn it into an OpenSpec task list. Write it for an engineer who is skilled but knows nothing about this codebase — every step names its files, its command, and its expected output.
argument-hint: "[change-id]"
allowed-tools: Bash(openspec list:*)
---

# Plan the work

**Change:** $ARGUMENTS

Changes available:

!`command -v openspec >/dev/null 2>&1 || { echo "openspec is not installed: npm install -g @fission-ai/openspec"; exit 0; }; [ -d openspec ] || { echo "this repository has no openspec/ yet: openspec init --tools claude"; exit 0; }; openspec list 2>&1`

If no change was named and exactly one is open, that is the one. If several
are, ask which — planning the wrong one is an hour nobody gets back.

The output is `openspec/changes/<change-id>/tasks.md`.

**Announce at the start:** "I'm using sdd-plan to write the implementation
plan."

Read `proposal.md` and the delta specs first. If there is no proposal, the
honest move is `sdd-brainstorm` — planning something nobody agreed to is work
that gets thrown away.

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

## The file

Use OpenSpec's own shape rather than inventing one:

```bash
openspec instructions tasks --change <change-id>
```

Sections with hierarchical numbering, checkboxes for tracking:

```markdown
# Tasks

## 1. Budget type
- [ ] 1.1 Write the failing test in `tests/budget.rs`
      Run: `cargo test budget::caps_attempts`
      Expected: FAIL, "no function named caps_attempts"
- [ ] 1.2 Implement `RetryBudget` in `src/budget.rs`
- [ ] 1.3 Run: `cargo test budget` — expected PASS
- [ ] 1.4 Commit
```

The numbering matters beyond tidiness: `sdd-execute` binds the run to a task by
its number and text, and an unnumbered list renumbers itself the moment one is
inserted.

Then:

```bash
openspec validate <change-id>
openspec status <change-id>
```

## Self-review against the spec

Run this yourself; it is a checklist, not a subagent dispatch.

1. **Coverage** — walk each requirement in the delta specs. Can you point at a
   task that implements it? List the gaps and add tasks for them.
2. **Placeholders** — scan for every pattern above.
3. **Name consistency** — does a function called `clear_layers` in task 3 stay
   `clear_layers` in task 7? A rename between tasks is a bug in the plan.

Fix inline and move on.

## Hand-off

Show the list and let it be corrected before anything is built. A plan is cheap
to change and expensive to have been wrong about.

Then offer the choice:

> Plan written to `openspec/changes/<id>/tasks.md`. Two ways to run it:
>
> **1. Subagent-driven (recommended)** — a fresh subagent per task, two-stage
> review between tasks, no waiting on me between steps.
>
> **2. Inline** — I work the tasks in this session, checkpointing at each.
>
> Which?

Either way the next skill is `sdd-execute`.
