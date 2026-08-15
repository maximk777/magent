#!/usr/bin/env bash
# Fails if the plugin as committed would not work on a clean clone.
#
# Two failures reached a live install before this existed, and neither could
# have been caught by the Rust tests:
#
#   1. The manifests invoked a bare `magent`. The plugin's bin/ is added to the
#      Bash tool's PATH only, so hooks and MCP servers could not find it.
#   2. plugin/.mcp.json was untracked, because a global gitignore rule for
#      .mcp.json applied. The file existed locally, so everything worked here
#      and nowhere else.
#   3. install.sh copied the new build over the old binary in place, while a
#      live session had that file mapped through its MCP server. Overwriting a
#      mapped executable invalidates its signature, and the installed copy was
#      SIGKILLed on every exec afterwards — every hook, every MCP start,
#      silently.
#
# Both are properties of the repository rather than of the code, so this runs
# without a compiler — which is also what makes it usable when the toolchain is
# unavailable.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"
git="${MAGENT_GIT:-git}"
failures=0

fail() {
  echo "FAIL: $1" >&2
  failures=$((failures + 1))
}

# --- everything the plugin needs is committed -------------------------------

for required in \
  plugin/.claude-plugin/plugin.json \
  plugin/hooks/hooks.json \
  plugin/.mcp.json
do
  if [ ! -f "$required" ]; then
    fail "$required is missing"
  elif ! "$git" ls-files --error-unmatch "$required" >/dev/null 2>&1; then
    fail "$required exists but is not tracked; a clone would not get it"
  fi
done

# --- commands resolve without relying on PATH -------------------------------

for manifest in plugin/hooks/hooks.json plugin/.mcp.json; do
  [ -f "$manifest" ] || continue
  if grep -q '"command"' "$manifest" && ! grep -q 'CLAUDE_PLUGIN_ROOT' "$manifest"; then
    fail "$manifest invokes a command without \${CLAUDE_PLUGIN_ROOT}; the plugin's bin/ reaches the Bash tool only"
  fi
done

# --- the manifests point at what the build produces -------------------------

if ! grep -q 'plugin/bin/magent' scripts/install.sh; then
  fail "install.sh no longer writes plugin/bin/magent"
fi

# The installed binary must actually run. It is not enough that it exists and
# is executable: an in-place overwrite leaves a file with both properties that
# the kernel refuses to start.
if [ -x plugin/bin/magent ] && ! plugin/bin/magent --version >/dev/null 2>&1; then
  fail "plugin/bin/magent exists but will not run; re-run scripts/install.sh"
fi

# The overwrite that caused it must stay fixed. A plain cp onto the live path
# reintroduces the SIGKILL and nothing else would notice until a hook died.
if grep -qE '^[[:space:]]*cp[[:space:]].*plugin/bin/magent[[:space:]]*$' scripts/install.sh; then
  fail "install.sh copies onto plugin/bin/magent in place; write beside it and mv, or macOS will kill the result"
fi

for manifest in plugin/hooks/hooks.json plugin/.mcp.json; do
  [ -f "$manifest" ] || continue
  if ! grep -q 'bin/magent' "$manifest"; then
    fail "$manifest does not point at the built binary"
  fi
done

# --- the events the design depends on are subscribed ------------------------

for event in SessionStart UserPromptSubmit PreCompact PostToolUse SessionEnd; do
  if ! grep -q "\"$event\"" plugin/hooks/hooks.json; then
    fail "plugin/hooks/hooks.json does not subscribe to $event"
  fi
done

if [ "$failures" -gt 0 ]; then
  echo >&2
  echo "$failures problem(s): a clean install of this plugin would not work." >&2
  exit 1
fi

echo "plugin ok: manifests are committed, resolve through the plugin root, and subscribe to every required event"
