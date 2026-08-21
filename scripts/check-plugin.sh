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

# The spec process is Magent's own: rows in the store, written and read through
# magent_propose, magent_specify, magent_plan, magent_archive and
# magent_changes. A skill or an agent that names openspec sends the reader to a
# CLI this project does not use, and in this repository following it would undo
# the design. The agents count twice over: a `description` is what a dispatching
# agent reads when choosing between them, so a stale one misdirects the caller
# before the file is even opened.
#
# Matched case-insensitively, because `OpenSpec` in prose is what a
# case-sensitive grep of these files missed. No exclusion for comments either:
# both kinds of file are prose all the way down, so there is no line in one
# where the word is merely being explained.
for path in plugin/skills/*/SKILL.md plugin/agents/*.md; do
  [ -f "$path" ] || continue
  if grep -qi 'openspec' "$path"; then
    fail "$path mentions openspec; the spec process lives in the store, so name magent_propose, magent_specify, magent_plan, magent_archive and magent_changes instead"
  fi
done

# sdd-execute is the only place that tells the model how to bind a run to a
# task. If the field names drift, the skill teaches a call that fails.
if [ -f plugin/skills/sdd-execute/SKILL.md ]; then
  for field in spec_change_id current_task task_done; do
    if ! grep -q "$field" plugin/skills/sdd-execute/SKILL.md; then
      fail "sdd-execute no longer mentions $field, which is how a run is bound to a task"
    fi
    if ! grep -rq "$field" crates/magent-mcp/src/lib.rs; then
      fail "sdd-execute teaches $field but the MCP server does not accept it"
    fi
  done
fi

# sdd-plan's example is the shape every future plan is copied from, and
# expected_output is a list of markers. It spent sixteen tasks as one line of
# prose that no output could ever contain, and nothing noticed — which is what
# this check is here to stop happening twice.
if [ -f plugin/skills/sdd-plan/SKILL.md ]; then
  if grep -q 'expected_output: "' plugin/skills/sdd-plan/SKILL.md; then
    fail "sdd-plan shows expected_output as a string; it is a list of markers, and a plan copied from that example is refused"
  fi
  if ! grep -q 'expected_output: \[' plugin/skills/sdd-plan/SKILL.md; then
    fail "sdd-plan no longer shows expected_output as a list, so nothing teaches the shape magent_plan accepts"
  fi
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

# --- the suite has one way to be run ----------------------------------------

# CI compiled and ran the whole workspace twice: once in release for the
# latency budget, and once again in debug inside the isolation script. That
# second cargo contended for target/, and the gate then failed with E0463 in a
# doctest -- a red that means nothing teaches people to ignore reds that do.
# One step now, so isolation is proved on the binaries that ship.
workflow=.github/workflows/ci.yml
if [ -f "$workflow" ]; then
  if grep -qE '^[[:space:]]*- run: cargo test' "$workflow"; then
    fail "$workflow runs cargo test directly; the suite goes through ./scripts/test.sh, which is what makes it isolated"
  fi
  if ! grep -q 'scripts/test.sh' "$workflow"; then
    fail "$workflow never invokes ./scripts/test.sh, so nothing proves the suite cannot reach a real profile"
  fi
fi

if [ ! -x scripts/test.sh ]; then
  fail "scripts/test.sh is missing or not executable; it is how this repository runs its suite"
elif ! "$git" ls-files --error-unmatch scripts/test.sh >/dev/null 2>&1; then
  fail "scripts/test.sh exists but is not tracked; a clone would not get it"
fi

# --- the README makes no claim that expires on its own ----------------------

# "Slice 3, in progress: workspaces" outlived the workspaces, the console and
# two slices of the spec-driven loop, and nothing noticed for weeks. A claim of
# incompleteness goes false without anybody editing it, which is exactly the
# class of failure this file exists for.
#
# Case-insensitively, for the reason the openspec check above gives. The list
# is deliberately short: it names claims Magent makes about itself, not every
# word a cautious writer might reach for. The stub words the planning skill
# forbids are kept out of it on purpose - the account of that skill quotes them
# as things a plan must never contain, and a pattern that failed on that
# quotation would be weakened at the first inconvenience.
if [ -f README.md ]; then
  while IFS= read -r hit; do
    fail "README.md dates itself: $hit"
  done < <(grep -niE '(in progress|slice [0-9]|coming soon|not yet)' README.md)
fi

# --- the Development section names commands that exist ----------------------

# The block listed five commands, two of which were the suite: once plainly and
# once again under a throwaway HOME. Those are one command now. A block listing
# four steps under a sentence that says five is the same drift as a stale slice
# number - nobody edits it, and it quietly stops being true.
if [ -f README.md ]; then
  if grep -q 'check-isolation.sh' README.md; then
    fail "README.md still names check-isolation.sh; the script is scripts/test.sh"
  fi
  if ! grep -q 'scripts/test.sh' README.md; then
    fail "README.md never names ./scripts/test.sh, so nothing tells a contributor how to run the suite"
  fi

  # The first fenced block under ## Development only. The section carries a
  # second one, for the ignored distill test, and counting it would make the
  # total answer to a different question than the sentence does.
  listed=$(awk '
    /^## Development$/ { section = 1 }
    section && /^```bash$/ && !seen { seen = 1; block = 1; next }
    block && /^```$/ { block = 0 }
    block && NF { n++ }
    END { print n + 0 }
  ' README.md)

  case "$listed" in
    3) word=three ;;
    4) word=four ;;
    5) word=five ;;
    *) word= ;;
  esac

  if [ -z "$word" ]; then
    fail "README.md's Development block lists $listed commands, a count no sentence in it can be checked against"
  elif ! grep -q "All $word before a commit" README.md; then
    fail "README.md's Development block lists $listed commands but does not say \"All $word before a commit\""
  fi
fi

if [ "$failures" -gt 0 ]; then
  echo >&2
  echo "$failures problem(s): a clean install of this plugin would not work." >&2
  exit 1
fi

echo "plugin ok: manifests are committed, resolve through the plugin root, and subscribe to every required event"
