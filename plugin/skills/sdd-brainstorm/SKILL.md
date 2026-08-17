---
name: sdd-brainstorm
description: "You MUST use this before any implementation work — building a feature, changing behaviour, adding functionality — including work that looks too simple to need it. Turns an idea into an agreed proposal in Magent's store through one-question-at-a-time dialogue, and refuses to write code until the design is approved."
argument-hint: "[what you want to build or change]"
allowed-tools: Bash(magent changes:*)
---

# Brainstorm a change

**Subject:** $ARGUMENTS

If that is empty, ask what they want to build before anything else — but ask it
as the first of the one-at-a-time questions below, not as a form.

Already in flight here:

!`magent changes 2>&1 || echo "magent is not installed: run ./scripts/install.sh in the Magent checkout"`

Read that before proposing anything. A change already open for this area is
either the thing being asked for, or a conflict worth naming now rather than at
merge time.

Turn an idea into a design, in dialogue, and land it as a change in Magent's
store.

This is the process from `superpowers:brainstorming` with two things added: the
artifacts are rows Magent's own verbs write rather than hand-rolled files, and
Magent's memory is consulted before the user is.

<HARD-GATE>
Do NOT write code, scaffold anything, or invoke an implementation skill until
you have presented a design and the user has approved it. This applies to every
change regardless of how simple it looks.
</HARD-GATE>

## Anti-pattern: "this is too simple to need a design"

Every change goes through this. A config flag, a one-function utility, a copy
fix. Simple changes are where unexamined assumptions cost the most, because
nobody looks twice. The design may be three sentences — but it is presented and
approved.

## The order

1. **Ask memory before asking the user.**

   ```
   magent_search "<the subject, in the user's words>"
   ```

   Much of what you are about to ask has been settled already: which service
   owns this, why the obvious approach was rejected last time, what the deploy
   constraint is. Asking again is worse than not asking — it spends the
   person's patience on something they already told you.

   Then read what is currently true. `magent_changes` returns the live
   capabilities alongside the changes in flight, and a capability named to it
   comes back with its requirements — that is the specification this change
   will amend. Then the repository's own instructions, `CLAUDE.md` or
   `AGENTS.md`, for the constraints that were never written as requirements.

2. **Explore the code.** Files, recent commits, the shape of what is there.

3. **Scope check.** If the request is several independent subsystems, say so
   now. Do not refine the details of something that needs decomposing first;
   help split it, then brainstorm the first piece.

4. **Ask clarifying questions — one at a time.**

   One question per message. Not a form, not a batch. Prefer multiple choice
   where it fits. You are after purpose, constraints, and what counts as done.

5. **Propose 2-3 approaches.** With trade-offs, leading with your
   recommendation and why. One real option plus two strawmen is a decision
   already made wearing a costume.

6. **Present the design in sections,** each scaled to its complexity, asking
   after each whether it holds so far. Cover architecture, the units and their
   boundaries, data flow, failure handling, and how it will be tested.

## Then write the change

Two verbs, in this order. The proposal first:

```
magent_propose { slug: "add-retry-budget",     # verb-first kebab-case
                 title: "Cap retries per job rather than per attempt",
                 classification: "bounded",
                 why: "A failing dependency currently multiplies into
                       thousands of attempts, and the queue drains hours
                       after the dependency comes back.",
                 what_changes: ["RetryBudget in src/budget.rs",
                                "the worker takes its budget from config"],
                 capabilities: ["worker/retry"] }
```

`capabilities` are the areas of the specification this change touches, and the
proposal is where they are declared. Then one `magent_specify` per capability,
carrying that capability's requirement deltas — each one `added`, `modified`,
`removed` or `renamed`, and each added or modified requirement with its full
text and at least one GIVEN/WHEN/THEN scenario. A capability the proposal did
not declare is refused; so is a requirement with no scenario, which is the
point — a requirement nobody can check against is an assertion.

To widen a change while it is still being written — before it is planned —
call `magent_propose` again with the same slug. That rewrites the proposal, and
it is the only way to declare a capability the first call missed.

There is nothing to validate afterwards. The verbs refuse the shapes a
validator used to catch, at the moment you write them.

## Self-review, then hand it to the user

Read what you wrote with fresh eyes:

1. **Placeholders** — any TBD, TODO, or vague requirement? Fix them.
2. **Consistency** — do sections contradict each other?
3. **Scope** — is this one change, or several wearing one name?
4. **Ambiguity** — could a requirement be read two ways? Pick one, say it.

Fix inline. Then stop and ask:

> Proposal recorded as `<slug>` — `magent_changes` naming it reads the whole of
> it back. Please review it before we plan the implementation.

Wait. If they want changes, make them and re-review.

## The terminal state

The only skill you invoke next is `sdd-plan`. Not an implementation skill, not
a language skill, not "just this one small thing first".

Before moving on, record what you established with `magent_remember`, citing
the file or conversation that settled it: a constraint discovered, an approach
rejected and why. The next change in this area should not rediscover it.
