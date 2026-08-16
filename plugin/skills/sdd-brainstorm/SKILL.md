---
name: sdd-brainstorm
description: "You MUST use this before any implementation work — building a feature, changing behaviour, adding functionality — including work that looks too simple to need it. Turns an idea into an agreed OpenSpec proposal through one-question-at-a-time dialogue, and refuses to write code until the design is approved."
argument-hint: "[what you want to build or change]"
allowed-tools: Bash(openspec list:*)
---

# Brainstorm a change

**Subject:** $ARGUMENTS

If that is empty, ask what they want to build before anything else — but ask it
as the first of the one-at-a-time questions below, not as a form.

Already in flight here:

!`command -v openspec >/dev/null 2>&1 || { echo "openspec is not installed: npm install -g @fission-ai/openspec"; exit 0; }; [ -d openspec ] || { echo "this repository has no openspec/ yet: openspec init --tools claude"; exit 0; }; openspec list 2>&1`

Read that before proposing anything. A change already open for this area is
either the thing being asked for, or a conflict worth naming now rather than at
merge time.

Turn an idea into a design, in dialogue, and land it as an OpenSpec change.

This is the process from `superpowers:brainstorming` with two things added: the
artifacts are OpenSpec's rather than hand-rolled, and Magent's memory is
consulted before the user is.

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

   Then read the project's own constraints: `openspec/project.md` if it exists,
   and `openspec/specs/` for what is currently true. Run `openspec list --specs`
   to see the domains.

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

Let OpenSpec own the layout — it has a CLI, and hand-rolled directories drift
from what `openspec validate` and `openspec archive` expect:

```bash
openspec new change <change-id>          # verb-first kebab-case: add-retry-budget
openspec instructions proposal --change <change-id>
```

Follow those instructions rather than a template remembered from somewhere.
Fill in `proposal.md` (intent, scope in and out, approach), the delta specs
under `changes/<change-id>/specs/<domain>/spec.md` as ADDED / MODIFIED /
REMOVED requirements with GIVEN/WHEN/THEN scenarios, and `design.md` when the
technical approach needs argument rather than statement.

```bash
openspec validate <change-id>
```

## Self-review, then hand it to the user

Read what you wrote with fresh eyes:

1. **Placeholders** — any TBD, TODO, or vague requirement? Fix them.
2. **Consistency** — do sections contradict each other?
3. **Scope** — is this one change, or several wearing one name?
4. **Ambiguity** — could a requirement be read two ways? Pick one, say it.

Fix inline. Then stop and ask:

> Proposal written to `openspec/changes/<id>/`. Please review it before we plan
> the implementation.

Wait. If they want changes, make them and re-review.

## The terminal state

The only skill you invoke next is `sdd-plan`. Not an implementation skill, not
a language skill, not "just this one small thing first".

Before moving on, record what you established with `magent_remember`, citing
the file or conversation that settled it: a constraint discovered, an approach
rejected and why. The next change in this area should not rediscover it.
