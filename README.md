# Magent

Durable task memory for coding agents. A task survives context compaction,
`/clear`, and being picked up in a new session — without you retelling it.

Magent is a sidecar, not another agent. It does not invoke models on your
behalf, own an agent loop, or replace native approvals. Claude Code keeps doing
all of that; Magent remembers what the context window forgets.

## What works today

Slice 1: context survival.

- A task is recorded from your first prompt, whether or not the model announces
  it.
- Files you change are recorded as they change.
- Before every compaction a checkpoint is written synchronously, carrying
  forward the reasoning from the previous one.
- After a compaction, a `/clear`, or in a fresh session, that state is injected
  back as a compact packet — around 600 bytes, not a transcript.
- Reasoning the model alone knows (decisions, alternatives rejected, what was
  verified) is distilled from the transcript in the background.

Slice 2: durable memory.

- Facts carry where they apply, how they may conflict, how far they are
  trusted, and what they were learned from. A new value supersedes the old one
  without destroying it.
- Retrieval is scoped: user-level facts travel everywhere, project facts stay
  in their project.
- A compact index of relevant facts is injected on each prompt — names and
  titles only — and the model opens what it needs with `magent_recall`.
- The existing corpus imports and exports losslessly, so the store is not a
  one-way door.

Slice 3, in progress: workspaces.

- A repository's toolchain is read from its manifests on first sight: language
  and version, package manager, declared scripts, linter configuration. All of
  it observed and cited, none of it claimed to have been run.
- Repositories can be gathered into a workspace, so what is true of a group of
  services reaches all of them. Imported memory filed under one project can be
  promoted to the whole group.
- Each repository carries a role, so infrastructure that deploys a dozen
  services is not treated like the service being worked on.

Slice 4: the console.

```bash
magent web
```

A local page for the half of curation automation cannot do: confirming what is
true, withdrawing what is not, correcting wording, folding duplicates together,
and moving a fact up to the workspace it actually belongs to.

Nothing there destroys anything. A withdrawal is reversible, a correction
supersedes rather than overwrites, and a merge leaves a relation pointing at
what was folded in — curation is where mistakes are made on purpose, and it
should never be frightening.

It binds to loopback only. It is an unauthenticated read-write view of a
personal memory, and it is not the daemon this design deliberately does
without: it opens the same file every hook opens, holds nothing, and can be
closed at any moment.

Spec-driven work: superpowers' process, OpenSpec's artifacts, Magent's thread.

The three parts are deliberately borrowed rather than invented, because both
sources encode practice that is expensive to rediscover.

**OpenSpec owns the artifacts.** `openspec new change`, `openspec instructions`,
`openspec validate`, `openspec archive` — the skills call the CLI rather than
hand-rolling directories that then drift from what it expects. The archive step
is the reason it is worth using at all: a completed change's ADDED, MODIFIED
and REMOVED requirements merge into `openspec/specs/`, which becomes what is
currently true for the next change.

**superpowers owns the process.** The hard gate before any code. One question
per message rather than a form. "Too simple to need a design" named as the
anti-pattern it is. Steps sized at two to five minutes — write the failing
test, watch it fail for the right reason, make it pass, commit. An explicit
list of plan failures: no "TBD", no "add error handling", no "similar to task
N". A fresh subagent per task with two-stage review, spec compliance before
code quality, and a status protocol that lets an implementer say it is stuck.

**Magent owns the thread between sessions.** Memory is searched before the user
is asked, because much of what would be asked has been settled already. And the
run points at the task, which is the one thing neither files nor subagents can
supply:

```
Task: make retries bounded
Change: add-retry-budget
On task: 1.2 Implement RetryBudget
Spec: openspec/changes/add-retry-budget/tasks.md
```

Three skills — `sdd-brainstorm`, `sdd-plan`, `sdd-execute` — and three
subagents the last one dispatches: `sdd-implementer`, `sdd-spec-reviewer`,
`sdd-code-reviewer`. The spec reviewer is told not to trust the implementer's
report and to read the code itself, which is where optimistic reports get
caught.

Languages, as a second plugin.

```
/plugin install magent-lang@magent
```

`magent-lang` carries `.lsp.json` for gopls, rust-analyzer,
typescript-language-server and pyright, plus one thin skill per language. It is
separate from `magent` on purpose: memory and language tooling are different
jobs, and someone who installs Magent for the former should not get four
attempts to start language servers they never asked for.

The skills are deliberately not tutorials. A model does not need to be told how
to write Go; it needs to be told that `cargo test` without `--workspace` runs
one crate and still says ok, that `npm install` in a pnpm repository rewrites
`node_modules`, that a bare `pytest` uses the wrong interpreter, and that
`go build` never compiles test files. Each one reads what the repository
declares first, and records what it establishes through `magent_remember`.

```bash
magent doctor
```

Reports which language servers this repository's toolchain wants and whether
they are installed, along with the profile it opened, its schema version, and
whether this workspace is grouped. A missing server is a finding rather than a
failure, so it stays usable in a script.

Setting up, by being asked rather than by being configured.

Magent registers a repository the first time a session opens in it, so nothing
has to be configured before it works. One thing it will not do silently is
group: fifty checkouts of one organisation side by side are one project, and
what is learned in any of them is true of all of them — but a parent directory
is also where unrelated work lives, so that cannot be inferred safely.

Instead the server says what it noticed. Its instructions are built per
connection, and when this workspace looks worth grouping they say so, in the
one place the model reads before doing anything. `magent_setup` then reports
what it found; with `apply` it asks the person to confirm through MCP
elicitation and only then groups.

Where the client cannot show a confirmation — Claude Code does not offer
elicitation to servers today — it refuses and prints the terminal command
instead. Regrouping fifty repositories with nobody asked is not an acceptable
fallback.

Reference sources.

```bash
magent deps add https://github.com/acme/thing --ref v1.2.0
magent deps list
```

A repository the workspace reads but does not work in gets a shallow checkout
under `~/.magent/deps/<host>/<org>/<project>@<ref>`, and `magent_deps` hands the
agent that path.

What this deliberately does not build is an index over those sources. The
agent already has grep and read, and against a local checkout those are faster
than a bespoke index, never stale, and cost nothing to maintain. The value here
is materialisation, not search: put the right revision at a known path and say
where it is.

## How it is put together

There is no daemon. Hook processes and the MCP server open one SQLite file
directly and rely on WAL plus a busy timeout; every mutation is idempotent on an
`operation_id`, so a retried hook cannot duplicate state.

The split between the two surfaces follows one rule — what MCP cannot guarantee:

| Surface | Carries | Why there |
| --- | --- | --- |
| Hooks | run identity, the file ledger, the pre-compaction checkpoint | they fire whether or not the model cooperates |
| MCP | instructions, tools, decisions and verification | it needs the model's own knowledge, and it is portable to other harnesses |

## Install

Requires Rust 1.96 and `git`.

```bash
git clone git@github.com:maximk777/magent.git
cd magent
./scripts/install.sh
```

The script builds a release binary and places it twice, because Claude Code and
a terminal resolve it differently:

- `plugin/bin/magent` is what the plugin's manifests invoke through
  `${CLAUDE_PLUGIN_ROOT}`. The plugin's `bin/` is added to the Bash tool's PATH
  only, never to the environment hooks and MCP servers are launched in, so the
  manifests use an explicit path.
- `~/.local/bin/magent` is a symlink, so `magent import` and the rest work from
  a terminal. Set `MAGENT_BIN_DIR` to put it elsewhere.

Then, in Claude Code:

```
/plugin marketplace add /path/to/magent
/plugin install magent@magent
```

To try it without installing:

```bash
claude --plugin-dir /path/to/magent/plugin
```

## Check it is working

```bash
magent hook session-start <<< '{"session_id":"probe","cwd":"'"$PWD"'"}'
```

Silence is correct when nothing is in flight. After some real work, the same
command prints the restoration packet.

State lives in `~/.magent/magent.db`, or in `$MAGENT_STATE_DIR` when set.

```bash
sqlite3 ~/.magent/magent.db 'SELECT task, status, stage FROM runs ORDER BY updated_at DESC LIMIT 5;'
```

## Bringing existing memory across

```bash
magent import --memory-dir ~/memory --codex-rollouts ~/.codex/memories/rollout_summaries
```

Re-running is safe: the import is idempotent. Nothing is written to the source
corpus.

To get it all back out as markdown:

```bash
magent export --into ~/memory-export
```

Import, export and import again returns the same facts and the same relations.
That round trip is covered by a test, because a store you cannot leave is a
store you should not adopt.

## Grouping repositories

```bash
magent workspace group --name wbbank ~/programming/wbbank/*/
magent workspace promote --namespace wbbank-project-expert --into wbbank
magent workspace list
```

Grouping is always explicit. Guessing it from directory layout is wrong often
enough — vendored checkouts, forks, scratch clones — that a wrong guess would
merge unrelated projects' memory, which is the failure that makes a memory
layer worth switching off.

Two checkouts of one repository collapse into a single identity, because they
are one project and its memory should not fragment across them.

## Cost

Distillation runs `claude --bare -p --model haiku`, so it goes through your
subscription rather than an API key. No second bill, no extra secret in a config
file. `--bare` is what stops it from recursing: without it the nested session
would load these same hooks, open a run, compact, and queue another
distillation.

## Failure behaviour

Magent being broken must cost you nothing. Every hook exits 0 and writes nothing
to stdout when it cannot do its job — a corrupt database, a missing binary, a
malformed payload. You lose the memory, not the session. This is covered by
tests rather than intent.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests never touch a real profile: they direct all state at temporary directories
through `MAGENT_STATE_DIR`.

One test needs an authenticated `claude` and a network, so it is excluded by
default:

```bash
cargo test -p magent-distill -- --ignored
```

## Licence

Apache-2.0.
