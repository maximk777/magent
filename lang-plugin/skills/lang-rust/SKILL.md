---
name: lang-rust
description: Use when working in a Rust repository — before running tests, clippy or fmt, and before claiming a change is verified. Establishes this repository's actual check commands from what it declares rather than assuming the defaults.
---

# Rust, in this repository

The defaults are wrong often enough to matter, and they fail quietly: a command
that runs and passes while checking nothing is worse than one that errors.

## Read first

- `Cargo.toml` — is there a `[workspace]` table? If so this is a workspace and
  bare `cargo test` runs **only the root package**. Note `members`.
- `rust-toolchain.toml` — the pinned version. `cargo` will honour it silently;
  a feature you expect may not exist on it.
- `[workspace.lints]` or `[lints]` — the lint level the repository already
  chose. Do not argue with it; `-D warnings` is the usual intent.
- `.cargo/config.toml` — a `[alias]` here can redefine what `cargo test` means.

Ask `magent_search` for what is already known about this repository's checks
before deriving them again.

## The commands, and the traps in them

| Intent | Command | Why not the shorter form |
|---|---|---|
| Tests | `cargo test --workspace` | Without `--workspace`, other crates' tests never run and the output still says ok |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | Without `--all-targets`, test and bench code is not linted; without `-D warnings` a warning is not a failure |
| Format | `cargo fmt --all --check` | `cargo fmt` rewrites files instead of reporting, which turns a check into a change |
| Docs | `cargo doc --workspace --no-deps` | Only when the crate publishes docs |

Order matters: `fmt --check` first, then `clippy`, then `test`. Clippy rewrites
nothing, but a formatting change after a clippy fix means running clippy again.

## Before saying it is done

Run the checks. Report the failures with their output rather than summarising
them. A test suite that was not run is not a passing test suite, and saying so
is cheaper than being found out.

If a check is slow enough that you skipped it, say which one and why.

## Worth remembering

When you establish something durable — this workspace's real test command, a
crate that must be tested with a feature flag, a lint the repository has
deliberately allowed — record it with `magent_remember`, citing the file that
says so. The next session should not have to derive it again.
