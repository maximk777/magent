#!/usr/bin/env bash
# Fails if the test suite writes to a real Magent profile.
#
# Test isolation is otherwise unenforced: a test that forgets
# MAGENT_STATE_DIR would silently write to working memory, and the damage
# would only surface later as facts that were never learned. Running the
# suite under a throwaway HOME makes that mistake impossible to miss.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

cd "$root"
HOME="$sandbox" cargo test --workspace "$@"

if [ -e "$sandbox/.magent" ]; then
  echo
  echo "FAIL: the suite wrote to \$HOME/.magent" >&2
  echo "Some test is missing MAGENT_STATE_DIR and would corrupt a real profile." >&2
  find "$sandbox/.magent" >&2
  exit 1
fi

echo
echo "isolation ok: no profile was created under a throwaway HOME"
