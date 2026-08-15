#!/usr/bin/env bash
# Builds Magent and places the binary where the Claude Code plugin can find it.
#
# Claude Code adds a plugin's bin/ to PATH while the plugin is enabled, so the
# hooks can call `magent` without absolute paths.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

echo "building..."
cargo build --release --bin magent

mkdir -p plugin/bin
# Copied rather than symlinked: a symlink into target/ breaks the moment
# someone runs cargo clean, and a hook that cannot start is a silent failure.
cp target/release/magent plugin/bin/magent

echo
echo "installed: $root/plugin/bin/magent"
"$root/plugin/bin/magent" --version
echo
echo "next, in Claude Code:"
echo "  /plugin marketplace add $root"
echo "  /plugin install magent@magent"
