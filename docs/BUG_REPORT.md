# KOD Deep-Dive Bug Report

> **Resolution status (2026-09-30).** This report was produced by an
> audit pass. Every finding was re-verified against the tree before
> being acted on, and the verification changed the picture for some:
>
> **Fixed:** BUG-01, 02, 04, 05, 06, 07, 08, 09, 11, 12, 13, 14,
> 15, 17, 18.
>
> **Stale / partially wrong:**
> - **BUG-03** — the version gate the report called "parsed then
>   discarded" already existed at the dispatch loop; only the
>   `#[allow(dead_code)]` on the field was stale. Fixed as a
>   doc/attribute cleanup.
> - **BUG-10 / BUG-16** — the provider-backing contradiction was
>   resolved by reading the actual manifests: `kod-provider-openai`
>   is a genuine `adk-model` wrapper; `kod-provider-anthropic` is a
>   native `wire` module with cache-control implemented (the crate's
>   own header was the stale part). `typesafe-ai-rs` is the Jev
>   client and unrelated to provider choice. Recorded in ADR-05.
> - The report's "no engine hook for LSP" and "no tree-sitter"
>   framings were checked and the plan items acted on separately.
>
> Kept as a record of the audit and its resolution, not as a
> standing list of open defects.

> Generated from a codebase audit following the project convention
> (manifest → README → ADR directory → crate entry points).
> `cargo check --message-format=short` exits 0 with **0 diagnostics**,
> so every finding below is a logic, policy, or documentation defect
> that the compiler does not catch.

## Scope & method

| Step | Artifact read |
|------|---------------|
| Manifest | `Cargo.toml` (workspace, 20 members), per-crate `Cargo.toml` |
| README | `README.md` (crate table, provider claims) |
| ADR | `docs/adr/ADR-04-adk-model-spike.md` (the only ADR) |
| Docs | `docs/ARCHITECTURE.md`, `deny.toml`, `clippy.toml` |
| Entry points | `kod-core/src/lib.rs`, `kod-types/src/lib.rs`, provider traits |
| Deep reads | `kod-error/src/error.rs`, `kod-risk/src/paths.rs`, `kod-tools/src/sandbox/landlock.rs`, `kod-core/src/serve.rs`, `kod-core/src/presence.rs`, `kod-core/src/jev.rs`, `kod-provider/src/request.rs`, `kod-provider/src/traits.rs` |

---

## Severity legend

- **P0** — security / data-loss / build-policy contradiction
- **P1** — incorrect behaviour on a real path
- **P2** — latent defect, misleading output, or drift with a real cost
- **P3** — cosmetic / documentation drift

---

## P0 findings

### BUG-01 — deny.toml forbids every git source, but the manifest has a git dependency

**Location:** `deny.toml` `[sources]` vs `Cargo.toml` `[workspace.dependencies]`

Evidence — deny.toml:

    [sources]
    unknown-registry = "deny"
    unknown-git = "deny"
    allow-registry = ["https://github.com/rust-lang/crates.io-index"]
    allow-git = []            # empty

Evidence — Cargo.toml:

    # TypeSafe AI / Jev client (git dependency, gilljon/typesafe-ai-rs v0.1.0).
    typesafe-ai-rs = { git = "https://github.com/elcoosp/typesafe-ai-rs", default-features = false, features = ["rustls-tls"] }

**Impact:** `cargo deny check` — the file's own comment calls it "the policy gate for the deny CI job" — will fail on `unknown-git` because `allow-git` is empty. Either the CI job is red, the job is skipped, or the gate is silently not enforced. The deny.toml header explicitly claims the opposite ("a dependency declared with a git source … would appear here and fail").

**Root cause:** The git dependency was added to Cargo.toml without updating the matching `allow-git` policy. The two files are edited independently and nothing links them.

**Fix:** add the allowed git source, e.g. `allow-git = ["https://github.com/elcoosp/typesafe-ai-rs"]`, or move the dependency to a pinned crates.io release.

---

### BUG-02 — The git dependency is unpinned (no rev, tag, or branch)

**Location:** `Cargo.toml`

    typesafe-ai-rs = { git = "https://github.com/elcoosp/typesafe-ai-rs", default-features = false, features = ["rustls-tls"] }

**Impact:** A git dependency with no `rev`/`tag` resolves to the remote default branch. `Cargo.lock` pins the commit for reproducible builds, but any lockfile refresh (a `cargo update`, a fresh checkout without the lock, a dependency bump) silently pulls whatever HEAD is at that moment. For a crate the harness calls on every decision (`kod-core/src/jev.rs`), a force-push upstream breaks builds with no local change.

**Root cause:** Git deps were added in the convenient form. The comment even names a version (`v0.1.0`) that the dependency line does not encode.

**Fix:** add `rev = "<sha>"` (or `tag = "v0.1.0"`).

---

### BUG-03 — Protocol version negotiation is declared but not enforced

**Location:** `crates/kod-core/src/serve.rs`

Evidence:

    pub const PROTOCOL_VERSION: u8 = 1;
    pub const MIN_PROTOCOL_VERSION: u8 = 1;
    pub const MAX_PROTOCOL_VERSION: u8 = 1;

    #[derive(Debug, Deserialize)]
    struct Request {
        #[serde(default)]
        #[allow(dead_code)]       // parsed, never read
        v: Option<u8>,
        ...
    }

The module doc promises: "the server ignores a missing version (older clients) but reserves the right to reject a higher one in a future release." The `v` field carries `#[allow(dead_code)]` — it is deserialized and then discarded. `MIN_PROTOCOL_VERSION` / `MAX_PROTOCOL_VERSION` are `pub` constants with no reader. There is a `hello` method in `PROTOCOL_METHODS` and the doc table, but no version check on dispatch.

**Impact:** A client that sends `"v": 99` (or `"v": 0`) is served as if it spoke v1. When the protocol does change, an old server will silently mis-handle a new client's frames instead of returning the named error the doc promises. This is the failure mode the constants exist to prevent.

**Root cause:** The negotiation scaffolding was written first (constants + field + doc), the check was deferred, and `#[allow(dead_code)]` suppressed the only signal that the field is unused. A blanket `allow(dead_code)` hid the unfinished work.

**Fix:** read `v` on the first request (or on `hello`) and reject `< MIN` / `> MAX` with a named error; drop the `allow(dead_code)` so the compiler flags the field the moment the check is removed.

---

## P1 findings

### BUG-04 — HTTP 408 is classified as a 0 ms timeout

**Location:** `crates/kod-error/src/error.rs`

    408 => KodError::ProviderTimeout { timeout_ms: 0 },

`KodError::ProviderTimeout` renders as `Provider timeout after {timeout_ms}ms`, and `is_recoverable()` / `is_retryable()` both treat `ProviderTimeout { .. }` as retryable. So a 408 produces the user-facing string **"Provider timeout after 0ms"** and is scheduled for retry.

**Impact:** A misleading message ("0ms") that suggests an instant failure, and a retry decision taken on a value the caller never measured. The 408 case has no real timeout to report; fabricating `0` is worse than a distinct variant or a prose message.

**Root cause:** The 408 arm was written to fit the existing `ProviderTimeout` shape, which requires a `timeout_ms`. `0` was used as a sentinel but never documented or special-cased in the `Display` impl.

**Fix:** add a dedicated variant (or handle `ProviderTimeout { timeout_ms: 0 }` in `Display` as "request timeout (server 408)").

---

### BUG-05 — KodError::rate_limited silently discards its status and body parameters

**Location:** `crates/kod-error/src/error.rs`

    pub fn rate_limited(retry_after: Option<Duration>, status: u16, body: &str) -> Self {
        let secs = retry_after.map(|d| d.as_secs()).unwrap_or(30);
        let _ = (status, body);          // discarded
        KodError::RateLimited { retry_after_secs: secs }
    }

**Impact:** The constructor's signature promises the caller's HTTP context is used (a body snippet, a status code); the body is thrown away. A 429 with a body explaining why (quota vs. concurrency) loses that context. Any future logging or triage that reads the error sees only `retry_after_secs`.

**Root cause:** The variant `RateLimited { retry_after_secs: u64 }` has no field for the body, so the constructor accepts data it cannot store. The `let _ = (status, body)` is the tell — a deliberate suppression rather than a design that lost the fields.

**Fix:** either drop the unused parameters from the signature, or add a `detail: String` field to the variant and populate it.

---

### BUG-06 — Landlock silently drops a path rule when open(2) fails

**Location:** `crates/kod-tools/src/sandbox/landlock.rs`, `add_path_rule`

    let fd_raw = unsafe { libc::open(cstr.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd_raw < 0 {
        let err = std::io::Error::last_os_error();
        tracing::debug!(... "landlock: could not open path for rule; skipping");
        return Ok(());          // rule silently not added
    }

The comment justifies skipping with: "the worktree may gain subdirectories after `restrict_self` … the profile built before that cannot name them." That justification only covers `ENOENT`. Any other `open` failure (`EACCES`, `ELOOP`, a path too long) drops the rule too, and the failure is logged at `debug!` — invisible at default log levels.

**Impact:** For a read-write path, dropping the rule is fail-closed (the sandboxed process simply cannot write there) — safe but surprising. For a read-only path, dropping the rule can make the sandboxed program fail to start (e.g. `/usr` unreadable), and the user sees an opaque exec failure rather than "sandbox rule for /usr was dropped." The module's own doc insists that "silently proceeding would be the 'illusion of security' failure mode the architecture document forbids" — yet a dropped rule is exactly a silent divergence from the declared profile.

**Root cause:** `ENOENT` (benign, expected) and every other errno (not benign) share one branch, and the branch is logged below the default visibility threshold.

**Fix:** match `err.kind() == ErrorKind::NotFound` → skip (warn); any other errno → return `Err(KodError::SandboxViolation)`. At minimum, escalate the log to `warn!`.

---

### BUG-07 — Pid-reuse makes a crashed session read as alive

**Location:** `crates/kod-core/src/presence.rs`, `process_alive`

    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 { return true; }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)

Liveness is a bare `kill(pid, 0)`. The marker file stores only the pid. If the OS recycles that pid for an unrelated process (common on long-lived, many-process machines), the crashed session's marker reports alive.

**Impact:** A caller that offers "recover the crashed session" will not offer it, and the session appears in the live list forever. The doc positions the pid check as "the cheapest liveness check a Unix gives you" — true, but it is not a correct one across pid reuse.

**Root cause:** Liveness keyed on a reusable identifier with no secondary discriminator (process start time, or an ownership token).

**Fix:** store `(pid, start_time)` in the marker (read `/proc/<pid>/stat` field 22 on Linux, `kinfo_proc` on macOS) and compare both, or write a per-run nonce and have the owner refresh it.

---

### BUG-08 — prepare_socket has a TOCTOU race between the liveness probe and the unlink

**Location:** `crates/kod-core/src/serve.rs`, `prepare_socket`

    match UnixStream::connect(path).await {
        Ok(_) => Err(InvalidState("another kod server is already listening")),
        Err(_) => {
            // "if a live server were there, connect would have succeeded"
            let _ = std::fs::remove_file(path);
            Ok(())
        }
    }

The comment's reasoning is sound only if the two syscalls are atomic. Between the failed `connect` (no listener yet) and the `remove_file`, a second `kod serve` can `bind` the path. The first process then unlinks the second's freshly bound socket.

**Impact:** Two servers both believe they own the socket; clients connect to one while the other's file is deleted. The loser's clients hang or the daemon becomes unreachable.

**Root cause:** Check-then-act on a filesystem path without a lock. `UnixListener::bind` already fails on an existing path, so the pre-emptive `remove_file` is the racy part.

**Fix:** drop the pre-emptive unlink. Bind directly; on `EADDRINUSE`, connect to test liveness and only then unlink-and-retry under an advisory lock (`flock` on a sibling `.lock` file).

---

## P2 findings

### BUG-09 — The README crate table and badge under-count the workspace by five crates

**Location:** `README.md`

- Badge: `Crates-15`.
- Architecture table rows: 15 (`kod-types` … `kod-cli`).
- `Cargo.toml` `members`: 20.

The table omits `kod-risk`, `kod-schema-dialect`, `kod-minimize`, `kod-stats`, and `kod-telemetry`.

**Impact:** A new contributor reading the README believes the workspace is 15 crates and never discovers `kod-risk` (path-danger classification) or `kod-schema-dialect` — both load-bearing. The architecture doc's crate-by-crate section is likewise missing them.

**Root cause:** Crates were added to Cargo.toml without updating the README table or the badge. Nothing enforces parity between the manifest and the doc.

**Fix:** regenerate the table from `Cargo.toml` `members`, or add a CI check that the two counts agree.

---

### BUG-10 — README and ARCHITECTURE.md still say the providers are backed by adk-model; the decision record says the project chose typesafe-ai-rs

**Location:** `README.md`, `docs/ARCHITECTURE.md`, `docs/adr/ADR-04-adk-model-spike.md`, `Cargo.toml`, provider crate manifests

The sources disagree:

| Source | Claim |
|--------|-------|
| `README.md` table | `kod-provider-openai` / `kod-provider-anthropic` "backed by adk-model" |
| `docs/ARCHITECTURE.md` | Same, plus SUPERSEDED markers around the Anthropic text |
| `docs/adr/ADR-04` | "Do not hand-edit", spike date 2026-09-17, evaluates adk-model 2.2, says A2 replaces it with a native wire.rs |
| `Cargo.toml` workspace.dependencies | declares `typesafe-ai-rs` (git), comment "TypeSafe AI / Jev client" |
| `crates/kod-provider-openai/Cargo.toml` | `adk-model = { version = "2.2", default-features = false, features = ["openai"] }` |
| `crates/kod-provider-anthropic/Cargo.toml` | `adk-model = { version = "2.2", features = ["anthropic"] }` |
| `crates/kod-core/Cargo.toml` | `typesafe-ai-rs = { workspace = true }` |
| `crates/kod-provider-anthropic/src/lib.rs` | "backed by adk-model" and ships `pub mod wire` (the native path ADR-04 said would replace it) |

**Actual state:** the provider crates depend on `adk-model 2.2`; `typesafe-ai-rs` is used only by `kod-core/src/jev.rs` (the Jev client). The Anthropic crate simultaneously documents itself as an adk-model wrapper and exports a native `wire` module, so "which path is live?" cannot be answered from the docs.

**Impact:** The decision record ("chose typesafe-ai-rs over adk-model for providers") is false against the manifest. A maintainer following the docs will edit the wrong crate, or believe the adk-model path was retired when it is the active one.

**Root cause:** ADR-04 is a script-generated spike, not a hand-written decision; its premise was never reconciled after the dependency set settled. The SUPERSEDED markers in ARCHITECTURE.md cover only the Anthropic text, not the OpenAI-provider or workspace-dependency claims.

**Fix:** either (a) rewrite the README table and ARCHITECTURE.md to state adk-model for both provider crates and typesafe-ai-rs only for kod-core::jev, and mark ADR-04's premise superseded; or (b) finish the wire.rs migration and remove adk-model. Pick one; today's state is both.

---

### BUG-11 — ARCHITECTURE.md data flow skips step 7

**Location:** `docs/ARCHITECTURE.md`, "Data Flow"

    1. User Input -> CLI/TUI
    2. Task Classification -> Core Engine
    3. Context Building -> Memory + Skills
    4. Task Routing -> Core Engine
    5. LLM Generation -> Provider
    6. Tool Execution -> Tools (if needed)
    8. Response -> CLI/TUI          <-- 7 missing

**Impact:** Trivial in isolation, but it is the authoritative architecture doc; a numbered list with a hole signals an edit that removed a step without renumbering, and readers cannot tell whether a step (tool-result feedback?) was dropped intentionally.

**Root cause:** Insertion/deletion without renumbering.

**Fix:** renumber, or restore the missing step (tool-result feedback to the provider, which the README's data-flow diagram does show).

---

### BUG-12 — clippy.toml sets too-many-arguments-threshold while the workspace lints allow the lint outright

**Location:** `clippy.toml` vs `Cargo.toml` `[workspace.lints.clippy]`

    # clippy.toml
    too-many-arguments-threshold = 8
    type-complexity-threshold = 300

    # Cargo.toml
    [workspace.lints.clippy]
    too_many_arguments = "allow"
    type_complexity = "allow"

Both lints are allowed workspace-wide, and every crate sets `[lints] workspace = true`, so the two thresholds in clippy.toml can never fire.

**Impact:** Dead configuration. A reader tuning clippy.toml changes nothing; the numbers imply a policy that does not exist.

**Root cause:** The workspace lint block was added after clippy.toml and allowed the lints, leaving the thresholds orphaned.

**Fix:** delete the two lines from clippy.toml, or drop the allows and keep the thresholds.

---

### BUG-13 — LlmProvider::capabilities's doc comment is orphaned onto the next method

**Location:** `crates/kod-provider/src/traits.rs`

    /// The default implementation collects generate_with_tools and
    /// replays it as chunks (no live tokens, but every implementor works).
    /// Providers with SSE support should override for real token streaming.
    /// Default capabilities. Providers that care — Anthropic for
    /// explicit cache support, OpenAI for pricing — override this.
    /// The conservative default disables nothing that works (tools on,
    /// streaming_tools off), which is the safe side of the trade.
    /// Delta §4.4: provider-native compaction.
    /// ...
    async fn native_compact(...) -> ... { Ok(None) }

    fn capabilities(&self) -> ProviderCapabilities {      // no doc
        ProviderCapabilities::conservative()
    }

The paragraph beginning "Default capabilities…" is the doc for `capabilities()`, but it sits inside the `stream_with_tools` doc block, immediately above `native_compact`'s doc, because `native_compact` was inserted between the doc and the function it documents. `capabilities()` itself is undocumented.

**Impact:** `cargo doc` renders the "default capabilities" prose as part of `native_compact`'s documentation — actively wrong — and `capabilities()` has no documentation. Anyone reading the generated docs is misled about both methods.

**Root cause:** A method was inserted between an existing doc comment and its item; rustdoc attaches the comment to the item that follows.

**Fix:** move the "Default capabilities…" paragraph down to sit directly above `fn capabilities`.

---

### BUG-14 — is_transient_transport_error's "500 " pattern depends on a trailing space

**Location:** `crates/kod-error/src/error.rs`

    const PATTERNS: &[&str] = &[
        "429", "rate limit", "too many requests", "overloaded", "temporarily", "try again",
        "500 ",                 // requires a trailing space
        "502", "503", "504",
        "bad gateway", "service unavailable", "gateway timeout",
        "internal server error", "server error 5",
        ...
    ];

`provider_status` builds 5xx messages as `server error {status}: {snippet}` — e.g. `server error 500: …`. That string contains `500:` not `500 `, so the `"500 "` entry never matches a message this workspace generates. A bare `"500"` (a message from a transport library that reports only the code) also fails: `"500"` has no trailing space.

HTTP 500 is still classified retryable — via the `"server error 5"` entry — so there is no live misbehaviour on the internal path. The bug is that the `"500 "` entry is a dead pattern whose shape (trailing space) makes it fragile: it will match `500 internal` but miss `500:` and `500`.

**Impact:** Latent. A future caller that formats a 500 as `got status 500` (no space after 500) would not be retried, even though the intent of the entry is clearly "retry HTTP 500".

**Root cause:** A prose-matched pattern list mixing substrings that need delimiters (`"429"`) with substrings that do not (`"500 "`), with no test pinning the bare-code form.

**Fix:** use `"500"` (matching the `"502"`/`"503"`/`"504"` entries) and add a test for the bare-code string.

---

### BUG-15 — Landlock PR_SET_NO_NEW_PRIVS is marked allow(dead_code) though it is used

**Location:** `crates/kod-tools/src/sandbox/landlock.rs`

    /// PR_SET_NO_NEW_PRIVS. Required before landlock_restrict_self; …
    #[allow(dead_code)]
    const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

The constant is read at `libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)`. The `allow(dead_code)` is unnecessary and its comment ("ignore the clippy warning about libc already declaring a constant of the same name") describes a different lint.

**Impact:** The attribute suppresses a warning that cannot fire, and the comment sends a reader looking for a problem that is not there. If the constant is ever genuinely orphaned, the allow hides it.

**Root cause:** A defensive attribute copied onto the wrong const, with a comment explaining a warning that is not the one suppressed.

**Fix:** remove the `#[allow(dead_code)]`.

---

## P3 findings

### BUG-16 — ADR-04 is the only ADR and is machine-generated with a stale premise

**Location:** `docs/adr/ADR-04-adk-model-spike.md`

Banner: `Generated by scripts/spike-adk-model.sh. Do not hand-edit.` Its title and body evaluate adk-model 2.2; the spike date is 2026-09-17. The directory contains no other ADR.

**Impact:** A reader looking for the project's architecture decisions finds a script output whose question ("does adk-model satisfy three capabilities for the D1 refactor?") has been overtaken by the provider crates pinning adk-model 2.2 directly and the Anthropic crate shipping the native wire.rs the spike said would replace it. There is no hand-written record of why the current provider stack is what it is.

**Root cause:** The spike was committed as the ADR, and the decision it fed was never written up as a human-readable record. This is the source of BUG-10's contradictory claims.

**Fix:** write a real ADR recording the current provider decision (which crate is authoritative for which provider, and why adk-model vs a native wire path), and mark the spike as generated input rather than the decision.

---

### BUG-17 — README's CLI command list omits serve

**Location:** `README.md`, "Usage -> CLI"

The command list has `chat tui agent swarm skills config models profile sessions map replay doctor init test completions`. It does not list `serve`, though ARCHITECTURE.md documents the Unix-socket daemon (`kod serve`, `--remote`) as delivered.

**Impact:** A user reading only the README cannot discover the daemon, which is the feature that lets two terminals share one engine.

**Root cause:** The README command list was not updated when serve landed.

**Fix:** add `serve` (with `--stop`) to the list and a short example.

---

### BUG-18 — KodError truncation test asserts the wrong bound

**Location:** `crates/kod-error/src/error.rs`

    let snippet = kod_types::strutil::truncate_chars(body, 300);

    #[test]
    fn provider_status_truncates_long_bodies() {
        let body = "x".repeat(1000);
        let err = KodError::provider_status(500, &body);
        assert!(msg.len() < 500, "message not truncated: {} chars", msg.len());
    }

The assertion passes only because `300 + prefix < 500`. It does not pin the truncation length; the test would still pass if the cap were raised to 499. A change to 400 would also pass while quietly doubling the amount of provider body echoed into logs.

**Impact:** The test claims to cover truncation but only bounds it loosely. A regression that changes the cap meaningfully would not be caught.

**Root cause:** The assertion was written against the rendered message length (prefix + snippet) rather than the snippet length.

**Fix:** assert the rendered message contains exactly 300 `x` characters, or expose/test the cap directly.

---

## Summary table

| ID | Severity | Area | One-line |
|----|----------|------|----------|
| BUG-01 | P0 | deny.toml / CI | allow-git = [] contradicts the git dependency |
| BUG-02 | P0 | Cargo.toml | Git dep unpinned (no rev/tag) |
| BUG-03 | P0 | kod-core/serve.rs | Protocol version parsed then discarded |
| BUG-04 | P1 | kod-error | HTTP 408 -> ProviderTimeout { 0ms } |
| BUG-05 | P1 | kod-error | rate_limited discards status/body |
| BUG-06 | P1 | kod-tools/landlock | Failed open(2) silently drops a rule |
| BUG-07 | P1 | kod-core/presence | Pid reuse reads a crashed session as alive |
| BUG-08 | P1 | kod-core/serve | TOCTOU between connect-probe and unlink |
| BUG-09 | P2 | README.md | Crate table/badge off by five |
| BUG-10 | P2 | docs + manifests | Provider backing (adk-model vs typesafe-ai-rs) contradicts itself |
| BUG-11 | P2 | ARCHITECTURE.md | Data-flow list skips step 7 |
| BUG-12 | P2 | clippy.toml | Thresholds for lints allowed workspace-wide |
| BUG-13 | P2 | kod-provider/traits.rs | capabilities doc attached to native_compact |
| BUG-14 | P2 | kod-error | "500 " pattern requires a trailing space |
| BUG-15 | P2 | kod-tools/landlock | allow(dead_code) on a used const |
| BUG-16 | P3 | docs/adr | Only ADR is generated, with a superseded premise |
| BUG-17 | P3 | README.md | serve missing from the command list |
| BUG-18 | P3 | kod-error tests | Truncation test does not pin the cap |

---

## Root-cause themes

1. **Two files that must agree, edited independently.** deny.toml vs Cargo.toml (BUG-01), README vs Cargo.toml members (BUG-09), README/ARCHITECTURE.md vs provider manifests (BUG-10), clippy.toml vs workspace lints (BUG-12). None has a parity check.
2. **Blanket allow hiding unfinished work.** `#[allow(dead_code)]` on the `v` field (BUG-03) and on `PR_SET_NO_NEW_PRIVS` (BUG-15) suppress the exact signal that the work is incomplete or the attribute is wrong.
3. **Sentinels used where a distinct state belongs.** `timeout_ms: 0` (BUG-04) and `let _ = (status, body)` (BUG-05) are values chosen to fit an existing shape rather than to carry meaning.
4. **Check-then-act without a lock.** prepare_socket's connect-then-unlink (BUG-08) and the pid-only liveness check (BUG-07).
5. **Generated artifacts standing in for decisions.** ADR-04 (BUG-16) is the origin of the provider-documentation contradiction (BUG-10).
