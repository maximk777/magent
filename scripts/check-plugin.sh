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

# --- the workflow skills ----------------------------------------------------

for skill in sdd-brainstorm sdd-plan sdd-execute; do
  path="plugin/skills/$skill/SKILL.md"
  if [ ! -f "$path" ]; then
    fail "$path is missing"
  elif ! "$git" ls-files --error-unmatch "$path" >/dev/null 2>&1; then
    fail "$path exists but is not tracked; a clone would not get it"
  elif ! head -5 "$path" | grep -q "^name: $skill\$"; then
    fail "$path does not name itself $skill; without it the directory name is used, and that changes on a marketplace update"
  fi
done

# The subagents sdd-execute dispatches have to exist, and to name themselves:
# without a name in the frontmatter the directory name is used, and dispatching
# by a name nothing answers to fails at the moment it is needed.
for agent in sdd-implementer sdd-spec-reviewer sdd-code-reviewer; do
  path="plugin/agents/$agent.md"
  if [ ! -f "$path" ]; then
    fail "$path is missing"
  elif ! "$git" ls-files --error-unmatch "$path" >/dev/null 2>&1; then
    fail "$path exists but is not tracked; a clone would not get it"
  elif ! head -5 "$path" | grep -q "^name: $agent\$"; then
    fail "$path does not name itself $agent"
  fi
  if ! grep -q "$agent" plugin/skills/sdd-execute/SKILL.md; then
    fail "sdd-execute never dispatches $agent, so it ships unused"
  fi
done

# The SDD skills are invoked as commands with a subject or a change id. Without
# the placeholder the argument still arrives, but appended as raw text rather
# than in a place the skill can frame.
for skill in sdd-brainstorm sdd-plan sdd-execute; do
  path="plugin/skills/$skill/SKILL.md"
  [ -f "$path" ] || continue
  grep -q 'argument-hint:' "$path" || fail "$path has no argument-hint, so /$skill autocompletes with no clue what to type"
  grep -q '\$ARGUMENTS' "$path" || fail "$path never places \$ARGUMENTS, so an argument arrives unframed"
done

# A bare `openspec init` exits with a list of thirty editor names and creates
# nothing. Any place that prints it as a suggestion has to print the working
# form, or the person follows a hint that fails.
# Comment lines are excluded: explaining why the bare form fails is not
# suggesting it, and a check that cannot tell the difference gets disabled.
for path in plugin/skills/sdd-*/SKILL.md crates/magent-cli/src/doctor.rs; do
  [ -f "$path" ] || continue
  if grep -v '^[[:space:]]*//' "$path" | grep 'openspec init' | grep -qv -- '--tools'; then
    fail "$path suggests a bare 'openspec init', which fails; use 'openspec init --tools claude'"
  fi
done

# sdd-execute is the only place that tells the model how to bind a run to a
# task. If the field names drift, the skill teaches a call that fails.
if [ -f plugin/skills/sdd-execute/SKILL.md ]; then
  for field in spec_change_id spec_paths current_task; do
    if ! grep -q "$field" plugin/skills/sdd-execute/SKILL.md; then
      fail "sdd-execute no longer mentions $field, which is how a run is bound to a task"
    fi
    if ! grep -rq "$field" crates/magent-mcp/src/lib.rs; then
      fail "sdd-execute teaches $field but the MCP server does not accept it"
    fi
  done
fi

# --- the language plugin ----------------------------------------------------

for required in \
  lang-plugin/.claude-plugin/plugin.json \
  lang-plugin/.lsp.json \
  lang-plugin/skills/lang-go/SKILL.md \
  lang-plugin/skills/lang-rust/SKILL.md \
  lang-plugin/skills/lang-typescript/SKILL.md \
  lang-plugin/skills/lang-python/SKILL.md
do
  if [ ! -f "$required" ]; then
    fail "$required is missing"
  elif ! "$git" ls-files --error-unmatch "$required" >/dev/null 2>&1; then
    fail "$required exists but is not tracked; a clone would not get it"
  fi
done

# A skill without a name in its frontmatter is invoked by directory name, which
# changes when a marketplace updates.
for skill in lang-plugin/skills/*/SKILL.md; do
  [ -f "$skill" ] || continue
  if ! head -5 "$skill" | grep -q '^name: '; then
    fail "$skill has no name in its frontmatter"
  fi
done

# Both plugins have to be offered, or one of them reaches nobody.
for plugin in '"magent"' '"magent-lang"'; do
  if ! grep -q "$plugin" .claude-plugin/marketplace.json; then
    fail "the marketplace does not list $plugin"
  fi
done

# doctor tells people to install a language server; the plugin is what launches
# it. Recommending something the plugin never starts sends someone to install a
# binary that then goes unused, so every server doctor names must appear here.
#
# Checked in that direction only: .lsp.json also carries nested "command" keys
# that are server options rather than servers — rust-analyzer's clippy setting
# is one — and matching those would report a failure that is not one.
if [ -f lang-plugin/.lsp.json ] && [ -f crates/magent-cli/src/doctor.rs ]; then
  for server in $(grep -oE 'command: "[a-z-]+"' crates/magent-cli/src/doctor.rs | cut -d'"' -f2); do
    if ! grep -q "\"$server\"" lang-plugin/.lsp.json; then
      fail "magent doctor recommends $server but the plugin never launches it"
    fi
  done
fi

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
