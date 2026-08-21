#!/usr/bin/env bash
# Runs this repository's test suite, isolated.
#
# Isolation is not a separate audit here - it is a property of the only way the
# suite is run. A test that forgets MAGENT_STATE_DIR would silently write to
# working memory, and the damage would surface later as facts that were never
# learned. Under a throwaway HOME that mistake is impossible to miss.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

# Only HOME moves. Cargo's own homes are pinned to the real ones, because they
# default to $HOME and would otherwise move with it — which meant every run
# re-downloaded the toolchain and all 188 dependencies into the sandbox and
# rebuilt them from scratch, minutes per run for a check that reads one path.
# Nothing here weakens it: the check is whether the suite creates $HOME/.magent,
# and a cargo cache is not a Magent profile.
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"

cd "$root"
HOME="$sandbox" CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
  cargo test --workspace "$@"

if [ -e "$sandbox/.magent" ]; then
  echo
  echo "FAIL: the suite wrote to \$HOME/.magent" >&2
  echo "Some test is missing MAGENT_STATE_DIR and would corrupt a real profile." >&2
  find "$sandbox/.magent" >&2
  exit 1
fi

echo
echo "isolation ok: no profile was created under a throwaway HOME"
