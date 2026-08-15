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

Memory of facts across projects, dependency indexing and the local Web UI are
later slices. See `docs/` for the architecture they build toward.

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

The script builds a release binary and places it in `plugin/bin/`, which Claude
Code adds to `PATH` while the plugin is enabled — so the hooks find `magent`
without absolute paths.

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
