#!/usr/bin/env bash
# Fail when the docs reference pages drift from the code.
#
# Every check here compares a page's list against the source that
# defines it. They exist because the reference pages are the ones a
# reader trusts to be exhaustive, and they rot silently: before this
# script, `reference/tools.mdx` documented 24 of 31 tools and
# `reference/tui-keys.mdx` listed 49 of 67 slash commands, and nothing
# noticed.
#
# Adding a tool, a CLI subcommand, or a slash command without a doc
# entry fails this script. That is the point.
set -uo pipefail

cd "$(dirname "$0")/.."
DOCS=kod-docs/src/content/docs
fail=0
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

note() { printf '%s\n' "$*"; }
problem() { printf '  MISSING: %s\n' "$*"; fail=1; }

# --- tools -----------------------------------------------------------------
# Names come from each tool's own `ToolDefinition`. Restricting the grep to
# the files that define tools keeps test fixtures and example strings out.
grep -hoE '(^|[[:space:]])name: "[a-z_0-9]+"' \
  crates/kod-tools/src/tools.rs \
  crates/kod-tools/src/edit_tool.rs \
  crates/kod-tools/src/batch.rs \
  crates/kod-tools/src/git.rs \
  crates/kod-tools/src/plan.rs \
  crates/kod-tools/src/todo.rs \
  crates/kod-tools/src/ask.rs \
  crates/kod-tools/src/check.rs \
  crates/kod-tools/src/web.rs \
  crates/kod-tools/src/tool_search.rs \
  crates/kod-tools/src/search.rs \
  crates/kod-core/src/jfind_tool.rs \
  crates/kod-core/src/hub_tool.rs \
  crates/kod-core/src/memory_tools.rs \
  crates/kod-core/src/advisor_tools.rs \
  crates/kod-core-tools/src/lsp_tools.rs \
  2>/dev/null \
  | sed -E 's/.*name: //; s/"//g' | sort -u > "$tmp/tools_code"

# `swarm_note` / `swarm_read` go through `base_definition("name", ...)`
# rather than a literal `name:` field, so the pattern above misses them.
grep -A1 'base_definition($' crates/kod-core/src/swarm_adapters.rs \
  | grep -oE '^\s+"[a-z_]+"' | tr -d ' "\t' | sort -u >> "$tmp/tools_code"
sort -u -o "$tmp/tools_code" "$tmp/tools_code"

grep -E '^### ' "$DOCS/reference/tools.mdx" \
  | grep -oE '`[a-z_]+`' | tr -d '`' | sort -u > "$tmp/tools_docs"

note "tools: $(wc -l < "$tmp/tools_code" | tr -d ' ') in code, $(wc -l < "$tmp/tools_docs" | tr -d ' ') in docs"
while read -r t; do
  grep -qx "$t" "$tmp/tools_docs" || problem "tool \`$t\` is not in reference/tools.mdx"
done < <(comm -23 "$tmp/tools_code" "$tmp/tools_docs")

# --- CLI subcommands -------------------------------------------------------
# The top-level table only: nested sub-tables (e.g. `kod skills list`) are
# documented in their own sections and would not match this list.
awk '/^pub enum Command \{/{f=1;next} f&&/^\}/{exit} f' crates/kod-cli/src/commands/mod.rs \
  | grep -E '^    [A-Z][A-Za-z0-9]*' \
  | sed 's/^    //; s/[ {(,].*//' \
  | sed -E 's/([a-z0-9])([A-Z])/\1-\2/g' | tr 'A-Z' 'a-z' | sort -u > "$tmp/cli_code"

# The glance table runs from its heading to the first `## ` section.
awk '/^## Commands at a glance/{f=1;next} f&&/^## /{exit} f' "$DOCS/reference/cli.mdx" \
  | grep -E '^\| `' | sed 's/^| `//; s/`.*//' | sort -u > "$tmp/cli_docs"

note "cli: $(wc -l < "$tmp/cli_code" | tr -d ' ') in code, $(wc -l < "$tmp/cli_docs" | tr -d ' ') in docs"
while read -r c; do
  # `__sandbox-exec` is registered in the table without the leading
  # underscores, because the double underscore marks it as hidden.
  # `kod __sandbox-exec` is the hidden variant: clap registers it as
  # `SandboxExec`, and the docs table spells it with the underscores a
  # user would actually type.
  case "$c" in
    sandbox-exec) grep -qx '__sandbox-exec' "$tmp/cli_docs" \
                    || problem "command \`__sandbox-exec\` is not in reference/cli.mdx" ;;
    *)            grep -qx "$c" "$tmp/cli_docs" \
                    || problem "command \`$c\` is not in reference/cli.mdx" ;;
  esac
done < <(comm -23 "$tmp/cli_code" "$tmp/cli_docs")

# --- slash commands --------------------------------------------------------
grep -oE 'name: "/[a-z-]+"' crates/kod-tui/src/app/mod.rs \
  | sed 's/name: "\///; s/"//' | sort -u > "$tmp/slash_code"

sed -n '/^## Slash commands/,$p' "$DOCS/reference/tui-keys.mdx" \
  | grep -oE '`/[a-z-]+' | tr -d '`/' | sort -u > "$tmp/slash_docs"

note "slash: $(wc -l < "$tmp/slash_code" | tr -d ' ') in code, $(wc -l < "$tmp/slash_docs" | tr -d ' ') in docs"
while read -r s; do
  grep -qx "$s" "$tmp/slash_docs" || problem "slash command \`/$s\` is not in reference/tui-keys.mdx"
done < <(comm -23 "$tmp/slash_code" "$tmp/slash_docs")

# --- the site builds -------------------------------------------------------
note "building the docs site..."
if (cd kod-docs && pnpm build >/dev/null 2>&1); then
  note "docs site: builds"
else
  printf '  the docs site does not build\n'
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  printf '\nDocs drift detected. Update the reference pages above.\n'
  exit 1
fi
printf '\nDocs match the code.\n'
