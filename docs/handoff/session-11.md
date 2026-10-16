# Handoff — kod codebase, session 11

## TL;DR

Continuation of session 10's bug-hunt arc, targeting the class that has
already produced real bugs twice: a config field that is parsed,
stored, and silently never read. HEAD moved b345586 to a0332b2.
Three real bugs fixed: ToolPolicy.network (18f365f),
redact_argument_paths (625b8b4), and ToolPolicy.domains (a0332b2).
All three are security-relevant and unreleased. Tests: 4248 passing,
3 skipped, 0 leaks.

## Current repo state

| | |
|---|---|
| Repo | /Users/adm/Documents/Repos/kod |
| Branch | main |
| HEAD | a0332b2 |
| Working tree | clean |
| Clippy | 0 warnings (workspace, all-targets, -D warnings) |
| cargo fmt --check | clean |
| Tests | 4248 passing, 3 skipped, 0 leaky |
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

### ToolPolicy.domains — parsed and dropped (REAL, fixed in a0332b2)

Declared at crates/kod-config/src/policy.rs:114, sibling of network.
ToolContext.allowed_domains is enforced in crates/kod-tools/src/web.rs
(a non-empty list restricts web_fetch to those hosts), but nothing
populated it. The doc comment at context.rs:739 claimed "the engine
from ToolPolicy.domains" did; that was false -- the third
assertion-of-behaviour-that-does-not-exist this class has produced. A
[tools.web_fetch] domains = [...] allow-list was parsed, stored, and
silently ignored.

Fix: PolicyEngine::web_fetch_allowed_domains() reads the effective
per-tool policy; KodEngine::web_fetch_allowed_domains_setting() returns
it (empty when no policy is installed); the per-call context builder
assigns ToolContext.allowed_domains. The inline subdomain match in
web.rs became domain_allowed() (case-insensitive, '.'-prefix so
notdocs.rs does not match docs.rs).

Tests: 2 accessor (kod-config), 2 engine wiring, 7 domain_allowed, and
an e2e that installs an allow-list and drives a real web_fetch round --
it refuses evil.example.com before any network call. The e2e was
proven to fail without the populate (the fetch reached DNS lookup
instead of the policy refusal).

## Where the next agent should pick up

1. Re-run the config-field scan (below). All three known hits are
   fixed; the next pass either finds a new field in this class or
   confirms none remain. The scan is mechanical and cheap.

2. v0.1.0 still carries the PolicyEngine security bug. Unchanged from
   session 10. Options: mark it superseded, delete it, or regenerate
   its notes from git-cliff.

3. Cut a release. Three security-relevant fixes (18f365f, 625b8b4,
   a0332b2) are unreleased. This is a habit, not a bug hunt.

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

Where to start: the security bug hunt found two classes worth a
follow-up pass. (1) Run the content-reader scan again after any new
tool lands — the source lint catches the shape, but a reader that
consults read_protection and forgets a *different* gate would slip
past. (2) The `xd://` dispatch is dead (default all-false permissions,
no production caller to `with_tool_permissions`): wire it through the
caller's real context or remove the scheme.

---

## Addendum — security bug hunt (later in session 11)

HEAD moved a0332b2 → 2e3ddf5. Two classes of security bug found, six
real fixes, plus one class-level invariant test and one source lint.
Tests: 4259 passing, 3 skipped.

### Class 1: read_file's numbered view vs the raw bytes it edits

The numbered view the model edits against must agree with the raw bytes
the edit store snapshots. Redaction runs on the display path; the store
guards the raw path; nothing kept them aligned. Four bugs:

- **37771f1 — speculative read skipped redaction + numbering.** The
  prefetch branch early-returned after the binary probe, so a
  `[policy.read_protection]` file in `Redact` mode leaked its secret
  whenever the engine had pre-read it, and `numbered: true` lost its
  `tag`.
- **64b631f — redaction notice shifted line numbers.** The numbered
  view regenerated from the notice-prefixed string, so file line 1
  showed as `2:`; `PUT 2.=2` edited the wrong raw line.
- **79774d0 — multi-line redaction changed the line count.** A PEM key
  collapsed to one `[REDACTED]` line but the view still emitted a tag;
  `PUT 1.=1` would clobber the whole key. Now refuses to number when
  the counts differ.
- **f86a08c — class-level invariant.** `read_file_content_invariants_hold_across_paths`
  drives {prefetched, disk} × {numbered, plain} × {single, multi, no
  secret} and asserts (1) no protected secret in the output, (2) a
  returned tag means exactly one `N:` row per raw line. Verified to
  catch all three shapes by re-injection.

### Class 2: content readers bypassing read_protection

`[policy.read_protection]` is installed by default; `read_file` honors
it, but three other readers gated only on `can_read` and returned a
protected file's content in the clear.

- **efe13f4 — grep (search_files) + git_diff.** A directory grep
  returned the secret line from a `.env`; `git diff path=.env` returned
  it on a `+` line.
- **121002e — read_file's internal-URL branch.** Checked `Refuse` but
  not `Redact`; `conflict://<id>` skipped the path check entirely and
  returned the block body verbatim.
- **43d80bb — source lint.** `content_reader_must_consult_read_protection`
  flags any production file that reads content + calls `can_read` but
  never mentions `read_protection`. One documented exemption
  (`lsp_tools.rs`).

### Class 3: internal-URL write/read bypassing the path gates

The write and read paths gated the filesystem only when
`url_path_target` returned `Some` — true for `conflict://<path>`, NOT
for a bare `conflict://<id>`.

- **2e3ddf5 — conflict://<id> bypassed can_write/can_read.** Write: a
  swarm agent with `write-globs = ["src/**"]` could resolve a conflict
  in a file outside its claim (write-set, `.git` deny, allowlist all
  bypassed). Read: `forbidden_paths` bypassed. Fix: `ProtocolHandler`
  gained `read_target_path` / `write_target_path` (default None for
  store schemes); the tools gate the mapped path before dispatching.

### Not a bug (reviewed)

- `xd://` dispatch builds a fresh `ToolContext` from only
  `tool_permissions` (never set) → all-false permissions → fail-closed.
  The feature is *dead*, not exploitable: `with_tool_permissions` has
  no production caller. Worth wiring or removing.

### Reproducible scans used

    # content readers that skip read_protection
    for f in $(find crates -name '*.rs' -not -path '*/target/*' -not -path '*/tests/*'); do
      grep -q 'read_to_string\|fs::read(' "$f" && grep -q 'can_read' "$f" \
        && ! grep -q 'read_protection' "$f" && echo "$f"
    done

    # pub fns with zero call sites (found the speculation early-return)
    # see the per-field scan in the main body; the same shape finds
    # a dead public fn: grep the name, count call-sites, read the
    # zero-caller ones.

---


---

## Addendum 2 — Jev outbound path (after v0.1.3)

HEAD moved 6f93df9 (v0.1.3) → a97cd0c. One security fix plus a
regression test.

### The leak

The Jev advisor builds state from transcript previews and sends it to
the TypeSafe judgment endpoint. The provider path scrubs those same
previews with the secret vault (`agent_loop.rs`); the Jev path never
consulted it. So a registered secret in a preview reached the endpoint
in the clear. `[jev] redact_paths` (a6b49ad) covered only *paths*; the
secret *values* were unhandled. Jev is opt-in (`enabled = false`),
which made this a smaller exposure than the provider leak in 53e480e,
not a different one.

### The fix (6260e02, a97cd0c)

A `RedactingJevDecider` decorator wraps whatever decider is installed
(`set_jev_client` / `set_jev_decider`), obfuscating every string in the
state before delegating. The `JevDecider` trait is the single choke
point — ~30 advisor call sites funnel through its four `evaluate_*`
methods, so one wrapper covers them all and no call site changed. The
vault slot moved to a shared `Arc` so a vault installed after the
client is still seen.

Three tests, each proven to fire pre-fix:
- `redacting_decorator_scrubs_secrets_in_state` (unit)
- `redacting_decorator_is_a_noop_without_a_vault`
- `installed_decider_receives_scrubbed_state` (engine wiring)
- `threshold_update_preserves_scrubbing` (the `with_thresholds`
  rebuild path must re-wrap, or `/jev tune set` silently disables it)

### Where the next agent should pick up

1. **Cut v0.1.4** — 6260e02 + a97cd0c are security-relevant and
   unreleased (Jev is opt-in, so this is lower priority than the
   v0.1.3 batch, but the rule stands: release when a security fix
   lands).
2. **`scan_text` / `scan_vendors` / `scan_connection_urls` still have
   no caller.** The `secret_sources` module documents vendor-pattern
   and connection-URL detection ("12 vendor regexes") and
   `BORROWED_PRIMITIVES.md` lists it as wired, but only `scan_env`
   runs. A vendor-shaped secret literal in a transcript is not
   auto-registered, so it is not obfuscated. This is the last known
   dead capability in the secret-handling area.
3. **v0.1.0** still carries the old PolicyEngine bug.

### The class, restated

Every security bug this session was one shape: a control enforced at
one path, absent at a sibling path. The two structural guards
(`read_file_content_invariants_hold_across_paths`, the
`content_reader_must_consult_read_protection` source lint) cover two
families. The Jev fix is a third structural guard by construction — the
decorator at the trait means no future advisor call site can bypass it.

---

## Addendum 2 — Jev outbound path (after v0.1.3)

HEAD moved 6f93df9 (v0.1.3) → a97cd0c. One security fix plus a
regression test.

### The leak

The Jev advisor builds state from transcript previews and sends it to
the TypeSafe judgment endpoint. The provider path scrubs those same
previews with the secret vault (`agent_loop.rs`); the Jev path never
consulted it. So a registered secret in a preview reached the endpoint
in the clear. `[jev] redact_paths` (a6b49ad) covered only *paths*; the
secret *values* were unhandled. Jev is opt-in (`enabled = false`),
which made this a smaller exposure than the provider leak in 53e480e,
not a different one.

### The fix (6260e02, a97cd0c)

A `RedactingJevDecider` decorator wraps whatever decider is installed
(`set_jev_client` / `set_jev_decider`), obfuscating every string in the
state before delegating. The `JevDecider` trait is the single choke
point — ~30 advisor call sites funnel through its four `evaluate_*`
methods, so one wrapper covers them all and no call site changed. The
vault slot moved to a shared `Arc` so a vault installed after the
client is still seen.

Three tests, each proven to fire pre-fix:
- `redacting_decorator_scrubs_secrets_in_state` (unit)
- `redacting_decorator_is_a_noop_without_a_vault`
- `installed_decider_receives_scrubbed_state` (engine wiring)
- `threshold_update_preserves_scrubbing` (the `with_thresholds`
  rebuild path must re-wrap, or `/jev tune set` silently disables it)

### Where the next agent should pick up

1. **Cut v0.1.4** — 6260e02 + a97cd0c are security-relevant and
   unreleased (Jev is opt-in, so this is lower priority than the
   v0.1.3 batch, but the rule stands: release when a security fix
   lands).
2. **`scan_text` / `scan_vendors` / `scan_connection_urls` still have
   no caller.** The `secret_sources` module documents vendor-pattern
   and connection-URL detection ("12 vendor regexes") and
   `BORROWED_PRIMITIVES.md` lists it as wired, but only `scan_env`
   runs. A vendor-shaped secret literal in a transcript is not
   auto-registered, so it is not obfuscated. This is the last known
   dead capability in the secret-handling area.
3. **v0.1.0** still carries the old PolicyEngine bug.

### The class, restated

Every security bug this session was one shape: a control enforced at
one path, absent at a sibling path. The two structural guards
(`read_file_content_invariants_hold_across_paths`, the
`content_reader_must_consult_read_protection` source lint) cover two
families. The Jev fix is a third structural guard by construction — the
decorator at the trait means no future advisor call site can bypass it.


---

## Addendum 3 — secret-vault coverage round

HEAD moved c330298 → ceb9173. Two more leaks in the secret vault's
coverage, both the same shape as the ones before: a source the vault
did not scan.

### The leaks

- **371e7b0 — vendor secret first seen in tool output.**
  `scan_text` (vendor regexes + connection-URL passwords) had no
  caller; the vault's only source was the env scan at install. A key
  that first appeared in a `read_file` result or shell output was never
  registered, so the outbound pass had nothing to substitute. Fixed by
  scanning every tool result before it enters the transcript
  (`register_secrets_in_text`, called from the dispatch loop).

- **df218b7 — vendor secret pasted by the user.** The tool-result scan
  covered tool output; the user's own message was covered by neither it
  nor the env scan. A key typed into the prompt reached the provider in
  the clear. Fixed by scanning the input in `prepare_turn`, the one
  choke point every entry point reaches.

- **ceb9173 — compaction rungs bypass the vault.** `handoff`, `remote`,
  and `snapcompact` build their own provider requests from the
  transcript, bypassing the obfuscation in `build_grounded_request`. A
  registered secret reached the summarizer raw, and the summary then
  carried it forward. Fixed by `obfuscated_transcript` at the engine's
  compaction ctx-construction site.

### Reviewed, not a leak

- `prewarm` sends only the cacheable system prefix (identity + repo
  map: file paths, not content), so no secret source.

### Where the next agent should pick up

1. **Cut v0.1.4** — five security-relevant commits since v0.1.3
   (`6260e02`, `a97cd0c`, `371e7b0`, `df218b7`, `ceb9173`).
2. **The vault-coverage shape is now well-tested but not exhaustively
   enumerated.** Every outbound provider path (`build_grounded_request`,
   compaction, Jev, prewarm) has been audited once. A future provider
   path — a new method that builds a `CompletionRequest` — must be
   checked the same way. There is no structural guard for this family
   yet; the candidate is a source lint that flags a `CompletionRequest`
   built outside `build_grounded_request` without an obfuscation call.
3. **v0.1.0** still carries the old PolicyEngine bug.


---

## Addendum 4 — disk-hygiene sweeps

HEAD moved ceb9173 → 601a670. One bug class, three instances: a
per-project or per-invocation directory under `~/.kod/` that nothing
ever pruned.

- **f63ca17 — checkpoint project dirs.** `enforce_retention` capped
  snapshots *within* a project (200), but the number of *projects* was
  unbounded — one `~/.kod/checkpoints/<hash-of-working-dir>/` per
  distinct working dir, including every test `TempDir`. A live install
  had **12,133 dirs, 235 MB**. Fix: `sweep_old_projects`, cap 1000 by
  mtime, called from shutdown.

- **c357ef6 — plan project dirs.** Identical shape:
  `~/.kod/plans/<hash>/` per working dir. 1,132 dirs live. Fix:
  `plan::sweep_old_plan_dirs`, cap 1000.

- **601a670 — session logs.** Flat `~/.kod/sessions/<ts>-<pid>-<seq>.jsonl`
  per invocation, never removed. 454 files live, **140 of them empty**
  (a recorder opened but never wrote). Fix: `sweep_old_session_logs`
  removes every empty `.jsonl` unconditionally and caps the rest at
  1000 by mtime.

All three run from the engine shutdown path (not per-write — the parent
readdir is O(entries)). Each has its own tests.

### Reviewed, not a bug

- `~/.kod/data/test.redb`: a stale Sep-4 artifact; `admin.rs` already
  moved the test DB to a scratch dir, so no active path recreates it.

### Where the next agent should pick up

1. **Cut v0.1.4** — eight security/hygiene commits since v0.1.3
   (`6260e02`, `a97cd0c`, `371e7b0`, `df218b7`, `ceb9173`, `f63ca17`,
   `c357ef6`, `601a670`).
2. **The unbounded-`~/.kod` shape may have more instances.** Audit any
   `home.join(".kod")` site that mints a per-invocation or per-project
   name. The three fixed here all went through shutdown now; a fourth
   would need the same. Candidate: a small helper that lists the
   `~/.kod` subdirs and their cleanup owner.
3. **v0.1.0** still carries the old PolicyEngine bug.


---

## Addendum 5 — v0.1.0 marked superseded

The v0.1.0 GitHub release carried a security bug (the TUI installed no
PolicyEngine, so the `standard` preset's write-approval and deny rules
were inert on the interactive surface). It shipped no notes beyond
GitHub's auto-line.

Done (not a code change — a release edit): prepended a `[!WARNING]`
banner to the v0.1.0 body pointing at v0.1.1 as the fix and the latest
release as the recommendation. The release stays published so its tag
and artifacts remain linkable. The original changelog line is kept
below the banner.

This was the last surviving item from earlier handoffs. Release history
no longer advertises a vulnerable build without a warning.


---

## Addendum 6 — MinimizeConfig discarded (246e305)

Another instance of the parsed-and-dropped class, this one a whole
config struct: `Minimizer::with_builtins_and_config(config)` did
`let _ = config`, and `Minimizer` held no config field. Both documented
knobs were inert:

- `enabled` ("whether the minimizer runs at all") — it ran regardless.
- `strip_ansi_globally` ("strip ANSI escape sequences from every
  capture, even when no def matches") — a command with no matching def
  returned the raw bytes with escapes intact, the common case for a
  command the def table does not know.

This is audit finding **F2a-8**, unfixed since the audit. The fix makes
`Minimizer` hold the config; `minimize()` returns the raw capture when
`enabled = false` and pre-strips ANSI when `strip_ansi_globally` is on.
`original_bytes` stays the true capture size (the naive pre-strip
corrupted it; the existing `minimized_reports_both_sizes` test caught
that). Two regression tests, each proven to fail pre-fix.

### Still open (feature decision, not a bug)

`MinimizeConfig` remains **embedder-only**: there is no `[minimize]`
block in `KodConfig`, and the engine always calls `with_builtins()`. So
no *user* can set `enabled`/`strip_ansi_globally` today. Wiring a
user-facing `[minimize]` block is a feature choice — the design doc
(`kod_borrow_from_oh_my_pi.md` §335) describes a much larger surface (76
built-ins, user-authored TOML defs) — so it wants a decision, not a
guess. The struct is now honest for an embedder that calls it directly.

### Reviewed, not bugs

- `kod-schema-dialect::recovery` (`classify_rejection`, `plan_retry`)
  has no external caller, but the module doc is explicit: "This module
  is layer 1 and the data types layers 2 and 3 need." Intentional
  future-work hooks, not a lying doc.
- F2a-13 (minimizer regex recompiled per call) is already fixed — a
  process-wide compile cache exists in `pipeline.rs::compile`.


---

## Addendum 7 — MinimizeConfig + keybindings path doc

HEAD moved da80630 → b4ff57d. Two fixes, both the same broad shape
(documented behaviour the code does not have).

- **246e305 — MinimizeConfig discarded.** `with_builtins_and_config`
  did `let _ = config`; the struct had no config field. Both knobs
  (`enabled`, `strip_ansi_globally`) were inert. Audit finding F2a-8.
  Fixed; two regression tests.

- **b4ff57d — keybindings module doc path wrong.** The module doc said
  `~/.kod/tui_keys.toml`; the code reads
  `dirs::config_dir()/kod/tui_keys.toml`. A user following the doc wrote
  a file nothing reads. `docs/tui-commands.md` already had the right
  path; the module doc was the outlier. The loader also had no test;
  factored into `load_bindings_from(paths)` and added four.

### Reviewed, not bugs

- The `docs/brainstorms/kod-code-audit-report.md` findings list is a
  **historical document**, not a live checklist. Spot-checks (F2a-11,
  F2a-13, F2e-8, F2f-23, F2h-14) are all fixed in the tree; the report's
  fix-status column is incomplete. Do not treat unmarked rows as open.
- `kod-schema-dialect::recovery` has no external caller; the module doc
  says it is "layer 1 and the data types layers 2 and 3 need" —
  intentional hooks.
- `StreamGuardConfig` fields read only inside their own module — my
  "zero external reads" scan false-positived on the defining file.


---

## Addendum 8 — command-risk classifier bypasses (0240eda, ae19734)

Three ways past the **unoverridable Catastrophic gate** (the policy
engine cannot widen it; only the user running the command themselves can
proceed). Found by probing `kod-risk::assess` with adversarial commands.

- **tee.** The `DEVICE_CAPABLE` doc named `tee` ("writes wherever it is
  pointed"), but the list omitted it — so `tee ~/.ssh/authorized_keys`
  and `echo x | tee /etc/sudoers` classified Safe. `tee` is not
  device-only, so it is now classified per-operand like a redirect
  target (0240eda).

- **Substituted operands.** `rm -rf $(echo ~)` and `rm -rf \`echo ~\``
  classified Safe. `classify_path` expands `~`/`$HOME`/`${HOME}` but
  returns Low for a token it cannot resolve, so the substitution slipped
  past the gate a literal `rm -rf ~` hits. Now an operand with `$` or a
  backtick that `classify_path` cannot resolve escalates to Confirm; a
  resolvable `$HOME` keeps Catastrophic (ae19734).

- **Mutating find.** `find ~ -delete` / `find ~ -exec rm {} +`
  classified Safe — `find` is not in DESTRUCTIVE and only the
  `find | xargs` pipe form was caught. Now a find with
  -delete/-exec/-execdir/-ok classifies its walk root (ae19734).

### Method

The probe was a throwaway `#[ignore]` test printing `assess()` for ~18
adversarial commands, then removed. That is the cheapest way to find
this class: enumerate the gate, feed it shapes the fix history did not
cover, read the levels. Worth repeating for any classifier/gate.

### Where the next agent should pick up

1. **More classifier shapes may slip past.** Other untested forms:
   `mv /Users/dev/a /dev/null`, `rsync --delete`, `python3 -c
   "open(...,'w')"`, `perl -e unlink`, `vim -c ':wq'`. A language
   interpreter running code the classifier cannot see is the same shape
   as `bash -c` — the `-c`/`-e` escalation exists for shells but not
   for python/perl/ruby.
2. **Cut a release** — five commits since v0.1.4 (three fixes, two
   docs).


---

## Addendum 9 — command-risk classifier, round 2 (8b7b230, a910a8f)

Two more Catastrophic-gate bypasses, found with the same probe method.

- **Interpreter code flags.** `python3 -c`, `perl -e`, `ruby -e`,
  `node -e`, `php -r`, `lua -e` run a code string the classifier cannot
  see — the same shape as `bash -c`, which already escalates. These did
  not, so `python3 -c "open('~/.ssh/id_rsa','w')"` classified Safe.
  Fix: an INTERPRETERS table; a match with the code flag escalates to
  Confirm. `python3 script.py` (a readable file) is unaffected (8b7b230).

- **In-place writers.** `sed -i`, `sort -o FILE`, `install` overwrite a
  named file — same blast radius as `cp`/`truncate` (both in
  DESTRUCTIVE), but unclassified, so `sed -i ... ~/.ssh/authorized_keys`
  was Safe while `cp ... ~/.ssh/authorized_keys` was Catastrophic. Fix:
  extract each writer's file operand and classify it (a910a8f).

### Reviewed, by-design narrowness (NOT bugs)

The probe also flagged `mv ~/a /dev/null`, `cp /dev/null ~/.bashrc`,
`rsync --delete`, `chmod -R 000`, `git clean -fdx /`, `tar --overwrite`,
`unzip -o`, `awk 'BEGIN{system(...)}'`, `vim -c ...` as Safe. These are
deliberate: Catastrophic is reserved for the home dir *itself*,
credential stores, system roots, and devices. A non-credential home
file rewritten by an unclassified tool is Low; the sandbox and the
policy engine are the defense-in-depth behind the blast-radius gate.
Escalating every unknown writer would over-flag and make the agent
useless. **Do not "fix" these** — the classifier's contract is "escalate
on the unresolvable, classify on the resolvable", not "deny everything
unlisted".

### The line that matters

A finding is a real bypass when a *classified* command's verdict differs
from an *unclassified* one doing the identical operation on the identical
target (`cp` vs `sed -i` on `~/.ssh/authorized_keys`). It is narrowness
when no classified command covers the operation at all (`chmod`, `rsync`)
— that is the sandbox's job, not the classifier's.

### Cut a release

Six commits since v0.1.4 (five fixes, one docs). The classifier fixes
are security-relevant; v0.1.5 is due.


---

## Addendum 10 — markdown fence parsing (30c4916, c07b053)

Two bugs in the TUI markdown code-block parser, both in the fence
open/close logic (a fresh area; earlier rounds covered config, the
vault, disk hygiene, keybindings, minimize, and the risk classifier).

- **A closing fence with trailing space did not close.** The close
  check trimmed only the *start* of the line, then required every
  remaining char to be a backtick. A close line `"``` "` (trailing
  space) failed that test, so the fence never closed and the rest of
  the document rendered as code. CommonMark allows trailing whitespace;
  fixed with `trim()` on both ends (30c4916).

- **A longer fence was closed by a shorter one.** The parser tracked no
  fence length: the open used `strip_prefix("```")` and the close
  accepted any 3+ run. A 4-backtick block exists precisely so it can
  contain a 3-backtick line; the inner ` ``` ` closed the outer block
  early. Fixed by recording the opening run's length and requiring a
  close of >= that length (c07b053).

Both proven with failing tests first. The `#[test]` attribute on the
anchor function was consumed by an insertion and restored (caught by
the dead-code lint).

### Where the next agent should pick up

1. **Cut v0.1.5** — eight commits since v0.1.4 (six fixes, two docs).
   The command-risk and markdown fixes are user-facing.
2. **Untested-parser shapes remain.** The markdown parser now handles
   fence length + trailing whitespace; other CommonMark gaps the review
   named (indented-code blocks, setext headers, nested lists) are still
   absent. Whether they matter depends on how much markdown the model
   emits; low priority unless a real reply renders wrong.


---

## Addendum 11 — the flaky TUI e2e (f6c28e0)

`tui_e2e::commands::save_and_load_round_trip_a_session` flaked three
times across this session, always in the full parallel run and always
passing in isolation.

**Cause:** after `/export --json` the test slept a fixed 800ms, then
asserted the output file exists. The write is an async command handler;
800ms is not reliable on a loaded machine, so the file simply had not
been written yet when the assert ran. Nothing to do with the markdown
or classifier fixes it happened to coincide with.

**Fix:** a `harness::wait_for_file(path, timeout)` poll helper (25ms
cadence, 20s ceiling) replaces the sleep-then-assert. Verified: 5x in
isolation, 3x72 TUI e2e under load, and the full workspace suite (4300
passing).

**The general rule for this suite:** a test that asserts on a command's
side effect must poll for it, not sleep a guessed interval. The other
file-exists assertions here are safe only because they are gated behind
a `wait_ci` on a terminal message that fires after the write — a new
assertion without that gate needs `wait_for_file`. `wait_for_absent`
was written for the negative case and removed when it turned out every
negative assertion already had a terminal-message gate.


---

## Addendum 12 — inject_intent_field skipped no-arg tools (ae406f6)

`Registry::get_definitions` documents "every schema gets an optional
`intent` field", but `inject_intent_field` returned early when a schema
had no `properties` object. A no-arg tool's schema is
`{"type": "object"}` (mcp_adapters.rs:315) — such a tool advertised
neither `intent` nor `accept_large_output`, so the model had no
advertised escape to request an oversized result in full; the context
guard withheld it silently.

Fixed: create an empty `properties` object when absent, then inject. A
`properties` present but non-object is left alone. Three tests; the
first proven to fail pre-fix.

### Reviewed, low-severity (not fixed)

Two bytes-vs-chars leftovers in the same class as the fixed F2e-7:

- `kod-stats::commit_mapreduce::cap_file_body` compares `body.len()`
  (bytes) against a char budget and reports "N chars elided" with a
  byte count.
- `kod-tui`'s `streamed_chars_this_turn` adds `chunk.len()` (bytes)
  to a field feeding a token estimate.

Both are display-only estimates; neither corrupts data. Worth a sweep
if the class recurs, but not urgent.

### Also stale

The `docs/brainstorms/kod-code-audit-report.md` findings list remains a
historical document. Spot-checked this round: F2a-9 (telemetry POST
timeout), F2h-13 (cleanse single-flight), F2e-7 (async_delivery
bytes/chars) are all fixed. Do not mine it for open items.
