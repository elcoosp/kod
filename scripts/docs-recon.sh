#!/usr/bin/env bash
# Dump ground truth for the areas the docs do not yet cover.
#
# This is a read-only survey, not a check: it prints what the code
# defines so a writer can document it without guessing. `check-docs.sh`
# is the gate; this is the source material.
set -uo pipefail
cd "$(dirname "$0")/.."

section() { printf '\n===== %s =====\n' "$*"; }

section "MCP: config, lifecycle, naming"
sed -n '1,60p' crates/kod-config/src/mcp.rs
echo "--- naming policy ---"
grep -rn 'mcp:' crates/kod-core/src/mcp_adapters.rs | head -5
echo "--- hot-reload / register ---"
grep -rn 'fn register\|hot.reload\|restart' crates/kod-mcp/src/client.rs | head -8
echo "--- http policy (ssrf for mcp) ---"
grep -n 'pub fn\|deny\|allow' crates/kod-mcp/src/http_policy.rs | head -10

section "MCP: tool cache"
grep -n 'pub fn\|pub struct' crates/kod-mcp/src/tool_cache.rs | head -10

section "Repo map: selection, caps, fallback"
grep -n 'MAX_FILE_BYTES\|const .*BYTES\|fn ast_or_regex\|fn scan' crates/kod-core-quality/src/repomap.rs | head -12
echo "--- what gets skipped ---"
grep -n 'skip\|ignore\|target/' crates/kod-core-quality/src/repomap.rs | head -8

section "Schema dialect: sanitize / recovery / memory"
sed -n '1,40p' crates/kod-schema-dialect/src/lib.rs
echo "--- public entry points ---"
grep -n 'pub fn' crates/kod-schema-dialect/src/sanitize.rs crates/kod-schema-dialect/src/recovery.rs | head -10

section "LSP: discovery, settle, deferred"
grep -n 'pub fn\|settle\|PATH\|which' crates/kod-lsp/src/manager.rs | head -14
echo "--- deferred diagnostics ---"
grep -n 'pub fn\|deferred' crates/kod-core-state/src/deferred_diagnostics.rs | head -10

section "Swarm: blackboard, hub, work pool, yield queue"
grep -n 'pub fn' crates/kod-swarm/src/blackboard.rs crates/kod-swarm/src/work_pool.rs crates/kod-swarm/src/yield_queue.rs | head -20

section "Session state: goals, decisions, citations, presence, sensitivity"
for f in goals decisions citations presence sensitivity; do
  echo "--- $f ---"
  grep -n 'pub fn' "crates/kod-core-state/src/$f.rs" 2>/dev/null | head -6
done

section "Cache accounting: journal, ledger, gauge"
for f in cache_journal cache_ledger context_gauge; do
  echo "--- $f ---"
  grep -n 'pub fn\|pub struct' "crates/kod-core-state/src/$f.rs" 2>/dev/null | head -6
done

section "Stats: if-bench, behavioral, commit"
grep -n 'pub fn' crates/kod-stats/src/if_bench.rs crates/kod-stats/src/behavioral.rs | head -12

section "Preflight / auto-thinking / prune / shake"
for f in preflight auto_thinking prune shake; do
  echo "--- $f ---"
  grep -n 'pub fn' "crates/kod-core-quality/src/$f.rs" 2>/dev/null | head -5
done
