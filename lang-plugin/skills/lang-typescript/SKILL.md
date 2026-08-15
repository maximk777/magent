---
name: lang-typescript
description: Use when working in a TypeScript or JavaScript repository — before installing, testing or type-checking, and before claiming a change is verified. Determines the package manager from the lockfile, which is the mistake that costs the most.
---

# TypeScript, in this repository

## The package manager is not a preference

Read the lockfile before running anything:

| Lockfile | Manager | Run scripts with |
|---|---|---|
| `bun.lock` / `bun.lockb` | bun | `bun run <script>` |
| `pnpm-lock.yaml` | pnpm | `pnpm <script>` |
| `yarn.lock` | yarn | `yarn <script>` |
| `package-lock.json` | npm | `npm run <script>` |

Running `npm install` in a pnpm or bun repository rewrites `node_modules` into a
layout the other manager did not intend and adds a second lockfile. It is slow
to notice and annoying to undo, and it is the single most common way to break
someone else's checkout.

## Read next

- `package.json` `scripts` — the repository's own commands. Prefer them over
  anything derived; `test` and `lint` here are the answer.
- `tsconfig.json` — `strict`, and whether `noEmit` is set. A `build` script does
  not necessarily type-check.
- Which test runner is actually present: vitest, jest, `bun test`, node's own
  `--test`. They differ in how they are invoked and in what a filter means.

Ask `magent_search` for what is already known about this repository first.

## The commands

| Intent | Command |
|---|---|
| Types | `<manager> exec tsc --noEmit` or the repository's `typecheck` script |
| Tests | whatever `scripts.test` says |
| Lint | whatever `scripts.lint` says |

Type-checking is separate from building and from testing. A test suite can pass
while the code does not type-check, because the runner may strip types rather
than check them.

## Before saying it is done

Run the checks. Report failures with their output. If you changed types, say
whether the type-check ran.

## Worth remembering

Record the package manager, the real test command and any script that wraps
non-obvious flags with `magent_remember`, citing `package.json` or the lockfile.
