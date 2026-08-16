---
name: sdd-brainstorm
description: Use at the start of any non-trivial change, before writing code — when the user describes something they want built, changed or fixed and the shape of the answer is not yet obvious. Turns a want into a written proposal, and refuses to propose a solution before the problem is stated.
---

# Brainstorm a change

The output is `openspec/changes/<change-id>/proposal.md`. The point is not the
file — it is that the thinking happens before the typing, and survives the
session that did it.

## Before asking anything, look

```
magent_search "<the subject, in the user's words>"
```

Much of what you are about to ask has been established already: which service
owns this, why the obvious approach was rejected last time, what the deploy
constraint is. Asking again is worse than not asking, because it spends the
person's patience on something you were told.

## Then ask, and do not skip this

Do not propose yet. State the problem first, in the user's terms:

- Who has this problem, and what do they do today instead?
- What would count as solved? What observable thing changes?
- What has been tried? Why did it not stick?
- What must not break? Which constraint is real and which is habit?

Ask the two or three of these that you cannot answer from the code and memory.
Not all of them, and not as a form.

## Explore, honestly

Write down at least two approaches that a competent person could actually
choose between. One real option and two strawmen is not exploration — it is a
decision already made, dressed up.

For each: what it costs, what it forecloses, and what would make you pick it.
If after that one is clearly right, say so and say why; a proposal that refuses
to recommend is work handed back.

## Write the proposal

`openspec/changes/<change-id>/proposal.md`, where the id is kebab-case and
verb-first: `add-retry-budget`, `split-ledger-writes`. Not `retry`, which names
a topic rather than a change.

```markdown
# <change-id>

## Why
The problem, in the terms the person used. Not the solution restated as a
problem.

## What changes
The approach chosen, in enough detail to disagree with.

## Alternatives
What else was considered and why it lost. This is the part that stops the
question being reopened in three weeks.

## Not doing
The neighbouring things this deliberately leaves alone.

## How we will know
What is observably different when this is done.
```

## Then stop

Do not start planning tasks in the same breath. The proposal is a thing to
agree on first; `sdd-plan` turns an agreed one into work.

Record what you established with `magent_remember` — a constraint discovered, a
rejected approach and why. The next change in this area should not rediscover
it.
