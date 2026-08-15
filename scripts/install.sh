#!/usr/bin/env bash
# Builds Magent and puts it where both the plugin and your shell can find it.
#
# Two separate placements, because Claude Code and a terminal resolve the
# binary differently:
#
#   plugin/bin/magent   what the plugin's manifests invoke through
#                       ${CLAUDE_PLUGIN_ROOT}. The plugin's bin/ is added to the
#                       Bash tool's PATH only, never to the environment hooks
#                       and MCP servers are launched in, so the manifests use an
#                       explicit path and this file has to exist.
#
#   ~/.local/bin/magent a symlink, so `magent import`, `magent workspace` and
#                       the rest work from a terminal. Skipped if that directory
#                       is not on PATH, since a link nobody can reach is worse
#                       than an honest message.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

bin_dir="${MAGENT_BIN_DIR:-$HOME/.local/bin}"

echo "building..."
cargo build --release --bin magent

mkdir -p plugin/bin
# Copied rather than symlinked: a symlink into target/ breaks the moment
# someone runs cargo clean, and a hook that cannot start is a silent failure.
cp target/release/magent plugin/bin/magent

echo
echo "plugin binary: $root/plugin/bin/magent"
"$root/plugin/bin/magent" --version

# --- the shell -------------------------------------------------------------

link="$bin_dir/magent"

if [ -d "$bin_dir" ] || mkdir -p "$bin_dir" 2>/dev/null; then
  # Symlinked here, unlike the plugin copy: this one should follow every
  # rebuild without the install script being run again.
  ln -sf "$root/plugin/bin/magent" "$link"
  echo "shell binary:  $link -> plugin/bin/magent"

  case ":${PATH}:" in
    *":${bin_dir}:"*)
      ;;
    *)
      echo
      echo "note: $bin_dir is not on your PATH, so \`magent\` will not resolve."
      echo "      add it, or set MAGENT_BIN_DIR to a directory that is."
      ;;
  esac
else
  echo "note: could not create $bin_dir; set MAGENT_BIN_DIR to choose another."
fi

echo
echo "next, in Claude Code:"
echo "  /plugin marketplace add $root"
echo "  /plugin install magent@magent"
