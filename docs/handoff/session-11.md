# Handoff — kod codebase, session 11

## TL;DR

Continuation of session 10's bug-hunt arc, targeting the class that has
already produced real bugs twice: a config field that is parsed,
stored, and silently never read. HEAD moved b345586 to 625b8b4.
Two real bugs fixed: ToolPolicy.network (18f365f) and
redact_argument_paths (625b8b4). A third candidate
(ToolPolicy.domains) is flagged for one more grep. Tests: 4236
passing, 3 skipped, 0 leaks.

## Current repo state

| | |
|---|---|
| Repo | /Users/adm/Documents/Repos/kod |
| Branch | main |
| HEAD | 625b8b4 |
| Working tree | clean |
| Clippy | 0 warnings (workspace, all-targets, -D warnings) |
| cargo fmt --check | clean |
| Tests | 4236 passing, 3 skipped, 0 leaky |
| Tags | v0.1.0 (997ab7f), v0.1.1 (193c8f7) |
| Disk | ~15 GiB free |

## Environment quirks

The seven from session 10 still hold (kache, never cargo clean, stale
.git/index.lock, no timeout(1), --no-fail-fast, --test it, rm -rf
target). Additions and corrections from this session:

1. The operator runs the same harness across 4 repos at once, so ps
   shows several ./wr1.sh processes (one per repo). Only kod's
   instance has kod as its working directory. Do not misread the
   multiple processes as concurrent writers to the same file.

   The harness can still deliver one script more than once (it
   re-fires on each write of wr1.sh). A script that writes a file or
   makes a commit should therefore be idempotent: guard the critical
   section with a mkdir lock (flock(1) is absent on macOS) and have
   the winner publish its result to a status file so a re-run can
   display it instead of redoing the work:

       LOCK=/tmp/foo.lock.d
       if ! mkdir "$LOCK" 2>/dev/null; then
         cat /tmp/foo-status.txt 2>/dev/null || echo "(still running)"
         exit 0
       fi
       trap 'rm -rf "$LOCK"' EXIT

2. Script size matters. A single script carrying a ~6 KB heredoc did
   not arrive intact: bash wrote a truncated file and the run still
   reported success. Two handoff writes were lost this way (a
   2094-byte truncated target, then a 1852-byte temp with no closing
   delimiter). Write long files in small chunks, each chunk its own
   script, each verified (byte count plus a content marker) before
   the next. Prefer temp-file-then-mv so a partial chunk never
   becomes the real file.

3. grep -c on a fixed string counts prose. grep -c 'network = false'
   returned 5 after the fix because the phrase appears in the
   enforcement, the rule format string, the test comment, the
   assertion message, and a TOML fixture. Test for the code predicate
   (network == Some(false)), not the phrase.

## What this session did

### 18f365f — ToolPolicy.network was parsed and dropped (REAL, fixed)

ToolPolicy.network's own doc comment promised it overrides the global
llm.network_access for this tool. Nothing read the field: decide()
consulted forbidden, binaries, forbidden_binaries, paths, and mode, but
never network. A user who wrote [tools.web_fetch] network = false had
the key parsed, stored, and silently ignored. Same shape as the
PolicyEngine-install bug (0362d30) and set_goal_token_budget.

Fix in crates/kod-config/src/policy.rs, in decide() (now around
lines 519-533): a network = false tool is denied outright. The
override may only narrow (turn a tool off); the global
llm.network_access permission remains the only grant path at the tool
context layer, so network = true cannot widen a read-only preset.

Three tests in policy::tests (now around lines 987, 1032, 1065):

- tool_policy_network_false_denies_the_tool
- tool_policy_network_true_cannot_widen
- tool_policy_network_none_falls_through_to_preset

Proven to fire on the pre-fix shape. With the enforcement block
removed and the file allowed to settle, nextest failed exactly one
test with:

    assertion `left == right` failed: network = false must deny even
    under a permissive preset; got rule "preset Yolo applies"
      left: Allow
     right: Deny

nextest exit 100. Restored, all three pass.

### redact_argument_paths — parsed and dropped (REAL, fixed in 625b8b4)

Declared at crates/kod-config/src/config.rs:65, inside RedactConfig.
The field's doc comment promises: when set, paths that match this list
are additionally redacted in ToolCall.arguments before being rendered
into the prompt; redacting arguments is stronger than redacting
results because the model cannot see the arguments at all, which is
what you want when a write_file is about to place a literal
credential.

Zero readers anywhere in the workspace. The only non-test occurrence
is the declaration itself. The documented example is
docs/brainstorms/KOD-IM-INTEGRATION-PLAN.md:317. It is not in
docs/config-reference.md.

The doc supports two readings and they give different code.

1. Wholesale: a tool call whose path argument matches the glob has its
   entire arguments object redacted.

2. Field-scoped: only the credential-bearing field (content for
   write_file, new_string for patch_file) is replaced.

Resolution (625b8b4): the doc's operative clause is "the model cannot
see the arguments at all", so the wholesale reading won. A matching
call keeps its tool name and id but its arguments become a single
explanatory JSON object -- valid JSON, so the provider wire layer and
any downstream parser still see a well-formed ToolCall. The switch is
independent of in_prompt: a normal coding session wants un-redacted
read_file results but not a credential in an outgoing write_file.

Wiring, as implemented:

- kod_config::policy::arguments_path_matches_redact_list is the single
  public matcher. It reuses extract_path_arg, resolve_path, and
  glob_matches, so argument redaction cannot drift from the policy
  engine's path semantics. Six tests pin it in kod-config.
- kod_core::engine::state::redact_tool_call_arguments is the pure loop
  (free function, unit-testable without an engine). Four tests.
- redact_messages_for_prompt (crates/kod-core/src/engine/state.rs)
  calls it as switch (a); in_prompt content redaction is switch (b).
- One e2e test builds an engine with KOD_CONFIG_DIR pointing at a
  config that sets redact_argument_paths, and asserts the secret is
  gone from the wire-visible arguments. Proven to fail without the
  wire-in (left: 0, right: 1; nextest rc=100).

- ChatMessage (crates/kod-types/src/message.rs:8) already carries
  tool_calls: Vec<ToolCall> and tool_call_id: Option<String>.
  ToolCall (crates/kod-types/src/tool.rs:134) has
  arguments: serde_json::Value.
- KodEngine::redact_messages_for_prompt
  (crates/kod-core/src/engine/state.rs:62) walks only m.content; it
  does not touch m.tool_calls. That is the natural hook: an
  argument-path pass would be a sibling loop over m.tool_calls, or a
  field added to the same function.
- Redactor::redact_json (crates/kod-types/src/redact.rs:189) already
  recurses objects and arrays in place. It is the right primitive for
  the wholesale reading.
- The glob semantics to match are glob_matches(pattern, path,
  working_dir) in crates/kod-config/src/policy.rs (the same three
  forms: verbatim, relative-to-workdir, and the **/-prefixed form).
- The list is on RedactConfig, reached via
  KodConfig::load_cached().security.redact.redact_argument_paths, the
  same cached-load pattern the existing redaction helpers use.

### ToolPolicy.domains — third candidate, verify before acting

Declared at crates/kod-config/src/policy.rs:114, sibling of network.
The non-test scan for .domains found exactly one occurrence: a doc
comment at crates/kod-tools/src/context.rs:739 that says "Populated by
the engine from ToolPolicy.domains when a ...". No production read of
the field was found. That doc comment may be another
assertion-of-behaviour-that-does-not-exist (the session-10 lint
no_bogus_watchdog_retry_event exists precisely because that shape
keeps recurring).

Do not assume. Run one grep for the struct-literal write (domains:)
and the field at context.rs:739, then decide. If the field is written
but never read, it is the same bug as network; if it is neither
written nor read, it is the same as redact_argument_paths.

## Where the next agent should pick up

1. Resolve ToolPolicy.domains with one more grep (above). Small.
   It is the last candidate in this class.

3. Re-run the config-field scan (below) after every fix in this
   class. The scan is mechanical and cheap, and each pass has found a
   real bug.

4. v0.1.0 still carries the PolicyEngine security bug. Unchanged from
   session 10. Options: mark it superseded, delete it, or regenerate
   its notes from git-cliff.

5. Cut a release whenever a security-relevant fix lands. 18f365f is
   security-relevant (a placebo security knob, now enforced) and is
   unreleased. This is a habit, not a bug hunt.

### The config-field scan (reproducible)

The scan that found network and redact_argument_paths:

    # 1. every pub <field>: in the config crate
    grep -rEn '^[[:space:]]*pub[[:space:]]+[a-z_][a-z0-9_]*[[:space:]]*:' \
      crates/kod-config/src --include='*.rs'

    # 2. for each field name, non-test reads outside the config crate
    for field in <names>; do
      n=$(grep -rEn "\.${field}([^a-zA-Z0-9_]|\$)" crates --include='*.rs' \
          --exclude-dir=target \
          | grep -v '^crates/kod-config/src/' \
          | grep -vE '/tests?/|/tests\.rs:|_test\.rs:|tests/it/' \
          | wc -l | tr -d ' ')
      echo "$field reads_outside_config=$n"
    done

Two known false-positive classes to filter by hand. First, a field
name that is a common English word (name, source, mode) matches
unrelated structs, so read the hits. Second, a field read only inside
crates/kod-config/src (like preset_explicit) is wired, not dead,
because the config crate consumes it before handing the effective
value out. The real hits are fields with zero reads outside the crate
and no internal read either (network, redact_argument_paths), or zero
reads anywhere (history_protected was this, and was fixed in cb5ba17;
confirm it stays fixed).

## Gotchas

1. tests/it is a separate crate. #[cfg(test)]-gated helpers do not
   reach it; use *_for_tests-suffixed accessors compiled
   unconditionally.

2. Source lints live in crates/kod-types/tests/it/source_lints.rs.
   They scan production lines only (stop at the first #[cfg(test)]).
   A lint that scans a window must bound it (session 10's watchdog
   lint overran its block into the wave loop and produced false
   positives).

3. Prove every regression test fails without the fix. Reintroduce the
   bug, watch the test fail, then restore. A proof that races the
   harness is not a proof.

4. cargo audit and cargo deny change under you (advisory-DB updates,
   not code changes).

5. grep -c on prose counts prose. Test for the code predicate, not
   the phrase.

6. Script size is a real hazard here: a single multi-KB heredoc did
   not arrive intact. Write long files in small verified chunks.

## What NOT to do

- Do not pkill watchexec (the operator runs the harness across 4
  repos).
- Do not delete ~/Library/Caches/kache.
- Do not run cargo clean. rm -rf target is the safe shape.
- Do not use head or tail to split file content across scripts.
- Do not use # comment lines inside bash scripts.
- Do not add speculative fixes without a reproducer, and for
  redact_argument_paths, not without a design decision.
- Do not run a whole-workspace build without checking free disk first.
- Do not write a long file with one giant heredoc; chunk it.

---

Where to start: decide the redact_argument_paths semantics (item 1).
It is the last known parsed-and-dropped field with a doc-established
promise; everything else in this class is either fixed or needs one
grep to confirm.
