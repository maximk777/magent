# Handing Magent to a fresh agent

Paste everything below the line into a new session. It is written to be read
by an agent that has never seen this repository.

Keep it current: it is checked into the repo precisely so it can be corrected
rather than re-derived. If something here is wrong, fix this file as part of
whatever proved it wrong.

---

You are picking up work on **Magent**, at `~/setup-claude`
(remote `git@github.com:maximk777/magent.git`, branch `main`). Read
`README.md` first — it is written to explain the design, not to advertise it.

## What Magent is

A local sidecar that gives a coding agent durable memory. Two halves, split by
what each can guarantee:

- **An MCP server** (`magent mcp`) — everything that needs the model's own
  knowledge: checkpoints, memory, reference sources, setup.
- **Hooks** (a Claude Code plugin) — everything that must happen whether or not
  the model cooperates: restoring context at session start, a deterministic
  checkpoint before compaction, the file ledger, closing sessions.

Everything lands in one SQLite file, `~/.magent/magent.db`. There is no daemon.

## Decisions that are settled — do not reopen them without a reason

- **No daemon.** Hook processes and the MCP server open the database directly
  and rely on WAL plus a busy timeout. Concurrency is proved by a test, not
  assumed.
- **The store is canonical; markdown is an export.** `magent export` round-trips.
- **Facts are never overwritten.** A conflict creates a revision and a relation
  (`supersedes` / `contradicts`). Curation is reversible by construction.
- **Detected facts say what a file declares and stop.** They never claim a
  command was run or a binary exists. Memory that asserts unverified things is
  worse than memory that stays quiet, because the agent acts on it confidently.
- **Nothing indexes dependency sources.** Reference repositories are checked
  out shallowly under `~/.magent/deps/` and the agent's own grep and read take
  over. An index would add staleness and a second copy on disk for no gain.
- **`tools/list_changed` is deliberately absent.** The tool set does not change
  with configuration, and hiding tools from an unconfigured workspace turns
  Magent off exactly when someone is new to it.
- **Grouping is proposed, never inferred silently.** A parent directory is also
  where unrelated work lives.
- **SDD borrows rather than invents.** Process from `superpowers`, the artifact
  model from `openspec`, and only the run-to-task binding is ours. If you are
  tempted to write a workflow prompt from first principles, read those two
  first — that mistake has already been made here once. Both are checked out
  under `~/.magent/deps/`; read them there.
- **The spec process lives in the store, not in `openspec/`.** The `openspec`
  CLI is not used: its validator runs to 842 lines because markdown lets an
  invalid document be written, and its own instructions admit a scenario under
  the wrong heading "will fail silently". As rows, that is unrepresentable.
  Markdown stays an export.

## How the work is done

- **Everything through TDD.** Write the test, run it, and confirm it fails *for
  the right reason* — not a typo, not a missing import. A test that fails to
  compile proves nothing.
- **Every gate green before a commit:**
  ```bash
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ./scripts/check-isolation.sh    # no test may touch the real ~/.magent
  ./scripts/check-plugin.sh       # the plugin would work on a clean clone
  ```
- **Tests must never touch the real profile.** Direct the store with
  `MAGENT_STATE_DIR`. A test that corrupted working memory would be
  unrecoverable, and `check-isolation.sh` exists because that nearly happened.
- **Verify against reality before claiming done.** Several defects here were
  found by running the thing on the real corpus and on real repositories, not
  by reasoning: `git clone --depth 1` silently ignoring `--depth` for a local
  path, a doctor printing a heading with nothing under it, a proposal offering
  to group the home directory under the user's own name.
- **Commit messages are short and say why.** A subject line, and a sentence or
  two only where the reason is not visible in the diff. Read `git log` before
  writing one; the register is consistent and deliberate. Reasoning that needs
  more room goes in a comment, next to what it explains — not into a wall of
  text at `git log`. **Never add a `Co-Authored-By` trailer.**

## Where things stand

53 commits, 395 tests, schema version 9.

```
crates/magent-core      domain types, I/O free
crates/magent-store     SQLite: facts, runs, git, grouping, deps, curation, setup
crates/magent-mcp       the stdio MCP server
crates/magent-distill   Distiller trait + a headless `claude -p` engine
crates/magent-web       the local console (Axum + Askama + HTMX, dark navy)
crates/magent-cli       magent hook/mcp/deps/doctor/web/import/export/workspace
plugin/                 the Magent plugin: hooks, .mcp.json, SDD skills, agents
lang-plugin/            the language plugin: .lsp.json, per-language skills
```

Working: context survival across compaction and `/clear`; the fact store with
FTS and prompt-driven retrieval; import and export of the user's real corpus
(211 facts); workspace grouping and promotion; reference checkouts; the
curation console on `magent web`; `magent doctor`; `magent_setup` with
elicitation; the SDD skills and three subagents.

The spec process is in the store: `magent_propose`, `magent_specify`,
`magent_plan`, `magent_archive` and `magent_changes`, fourteen tools in all,
with a change addressable by its slug. Every write is one operation and either
lands whole or not at all.

**Not built, and the gap that matters most:** nothing closes a task. `plan`
writes tasks as `pending`, no verb moves them, and `archive` refuses while any
is open — so the loop reaches `planned` and stops. The decision was that
`magent_checkpoint` ticks a task off with its evidence, and no task in the plan
ever built it. Running the loop on itself is what found this; eight reviews did
not, because each looked at its own piece and the hole was between them.

Also not built: the three SDD skills still call the `openspec` CLI and are
wrong in two known places (`openspec status <id>` is `--change <id>`, and
`openspec/project.md` is never created — project context lives in
`config.yaml`). Distillation now runs, but has not been exercised at length.
There is no `project.md` constitution.

## Getting oriented fast

```bash
cd ~/setup-claude
magent doctor                 # profile, schema, workspace, toolchain
magent web                    # the console, on 127.0.0.1:7717
git log --oneline | head -25  # the design argument, in order
```

The user's real profile is live at `~/.magent/magent.db` and holds their actual
memory. **Read it freely; do not write to it to try something out.** Use a
temporary `MAGENT_STATE_DIR` for experiments.

## Working with Maxim

He works in Russian and expects replies in Russian. He asks for architecture to
be worked through in dialogue rather than delivered, wants alternatives
explored out loud, and will push back when something has been asserted without
being checked — correctly, on at least one occasion here. When he does, go and
look rather than defend.
