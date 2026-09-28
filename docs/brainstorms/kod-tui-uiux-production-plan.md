# kod TUI — UI/UX Production Plan

**Executors:** this document is written for an AI coding agent (or a human) that follows instructions literally. Every task lists the exact file, the exact code to remove, the exact code to insert, the tests to touch, and the command that proves it worked.
**Repo:** `github.com/elcoosp/kod` — Rust workspace, edition 2024. All line numbers verified at commit `eb39247` ("feat: WIP").
**Scope:** `crates/kod-tui` (the TUI), `crates/kod-core/src/engine/mod.rs` (the tool-marker wire protocol the TUI consumes), `crates/kod-core/src/acp.rs` (marker consumer), plus the tests that pin current behavior.
**Out of scope:** provider/retry code (covered by the separate `tab-bridge-kod-production-plan.md`), swarm logic, config schema.

**Goal state — the five requirements this plan is graded against:**

1. **No clutter** — the chat shows what happened, not bookkeeping. The dim separator rule appears once per user turn, not between every pair of messages. The header carries 4–7 segments ordered by importance, never a theme-name badge. One-shot notices ("turn took 34s", repeated taint banners, "[skills] used: …") stop landing in the transcript.
2. **Tool calls show less output** — a finished tool row shows its header plus a 4-line body preview by default (`o` expands to the full capped summary). Errors always render in full.
3. **No mangled output when errors happen** — error advice strings contain no runs of internal spaces; a multi-line provider error renders as one collapsed summary line + advice; a failed turn keeps the partial answer it already streamed (like cancel does); approval dialogs never clip their key legend.
4. **Everything at its proper place** — parallel tool calls fill *their own* rows: the running-indicator update and the done-marker target the row created for that exact call id, not "the newest row that looks similar".
5. **No duplicates** — a tool error is displayed exactly once (the auto-expanded ✗ tool row; the extra `Tool … failed:` system line is gone). A task-end fallback that arrives after the live done-marker is a no-op keyed by call id, not a fuzzy text match that can append a second row.

---

## Part 0 — System map (read this first, do not modify anything yet)

### 0.1 Render pipeline (one frame)

```
TuiLoop::run
  └─ main_loop (crates/kod-tui/src/main_loop.rs:627)
       ├─ event_handler.next_event()            → Event (event.rs)
       ├─ handle_event(event)                   → mutates KodApp state (app/*.rs)
       └─ render() (main_loop.rs:658)           → terminal.draw
            │  Layout vertical:
            │    [0] HeaderWidget   (1 row)      ui/header.rs
            │    [1] ChatWidget     (Min 1)      ui/chat.rs      ← all messages
            │    [2] CompletionsWidget (popup)   (only when completing)
            │    [3] StatusWidget   (1 row)      ui/status.rs
            │    [4] InputWidget                 ui/input.rs
            │  Overlays: HelpWidget, ApprovalWidget, PaletteWidget, QuestionWidget
            ▼
      ChatWidget::render (ui/chat.rs:343)
        ├─ sort messages by `sequence`
        ├─ probe pass: cached per-message row counts → scrollbar decision
        ├─ final pass: message_lines() per message
        │    ├─ Tool      → "⚙ header" + body preview (TOOL_DISPLAY_LINES=12) or "✗ …" for errors
        │    ├─ Assistant → markdown render_cache → "╭─ ai ─╮" frame
        │    └─ User/Sys  → "you"/"sys" prefix + wrap_text
        └─ Paragraph::scroll to the bottom-most offset
```

### 0.2 The tool-call event flow (where duplicates and misplacement come from)

The engine reports tool calls to the TUI through **marker chunks** smuggled inside the
streaming-text channel (`crates/kod-core/src/engine/mod.rs:116-202`). Markers are `\0`-delimited
strings parsed back into `Event`s by the pump in `main_loop.rs:1355-1372`:

```
engine run_streaming_loop                           TUI pump (main_loop.rs)
  StreamChunk::ToolCallStart {index,id,name}  ──▶  tool_start_marker(name)             → Event::ToolStarted(name)
  after args assembled:   per call              ──▶  tool_args_marker(brief)             → Event::ToolProgress(display)
  run_tool_calls:         per call, when done   ──▶  tool_done_marker(header,summary,ms) → Event::ToolCompletedWithDuration(...)
  after the whole loop returns                   ──▶  (task-end) Event::ToolCompleted(header, summary)
                                                       for EVERY call again (main_loop.rs:1451-1466)
```

**The markers carry no call identity.** The TUI therefore matches rows by *text heuristics*
(`app/tools.rs:95-101, 175-196`): "the newest Running row whose name is contained in the header
(in either direction)". With two parallel `read_file` calls this mis-attributes results, can
overwrite the wrong row, and can append a duplicate row. The task-end fallback re-emits every
result and relies on the same heuristics for idempotency.

`ToolCall.id` — the provider's tool-call id (`Option<String>`) — already exists on the engine's
`ToolCall` struct and is available at all three marker emission points. The plan threads it
through the protocol (v2) and keys the TUI's row registry on it. Consumers that only test
`is_some()` (`kod-cli/src/commands/chat.rs:515`, `prompt.rs:195`, `kod-core/src/swarm_runner.rs:1234`)
keep working unchanged; the two destructuring consumers (`kod-core/src/acp.rs:562,612`, engine +
kod-tui tests) are updated in the same commit.

### 0.3 Error surfaces today (where mangling and double-display come from)

| Surface | Code | Behavior today |
|---|---|---|
| Tool failure row | `app/tools.rs:198-231` | `✗ header` + full `Error: …` body, auto-expanded. Single good source. |
| Task-end error duplicate | `main_loop.rs:989-997` and `1009-1015` | **Also** pushes `Tool \`X\` failed: Error: …` as a system message → the user sees the same error twice. |
| Turn-level failure | `app/streaming.rs:185-203` (`fail_generation`) | Discards the partial streamed answer (cancel keeps it), settles running rows with the **raw error string** as body, pushes `friendly_error`. |
| `friendly_error` advice | `app/streaming.rs:212-270` | Three of the seven advice literals contain **runs of 14+ internal spaces** (source-line-wrap artifacts). Raw provider errors are shown in full, multi-line. |
| Engine system notices | `event.rs:98` `Event::System(EventPriority, String)` | **No `handle_event` arm exists** — the phase-change hint (`main_loop.rs:850-856`) and the corrupt-approval-batch warning (`main_loop.rs:1323-1333`) are sent and silently dropped (`_ => {}` at `main_loop.rs:1107`). |
| Status line | `ui/status.rs:99-140` | During generation repeats the live tool row's brief (`tool: cargo test…`) that the chat already shows; `active_tool()` arm renders a nonsense `0/0` pair. |
| Approval popup | `ui/approval.rs:174-189` | `body_h = lines.len() + 2` ignores `Wrap` growth — long diff lines wrap and clip the key legend at the bottom. |
| Plain-text wrapping | `ui/chat.rs:30-51` (`wrap_text`) | Hard-breaks at `width` chars **mid-word** for user/system/tool bodies — asymmetric with the markdown path (`markdown.rs:607-729`), which wraps at word boundaries. |

### 0.4 Defect table (full list — all fixed in Part 2)

| ID | Sev | Requirement | One-line summary |
|----|-----|-------------|------------------|
| U1 | P0 | no duplicates | Tool error rendered twice: auto-expanded ✗ row **and** `Tool … failed:` system line |
| U2 | P0 | proper place | Tool markers carry no call id → parallel calls fill the wrong rows / duplicate rows (text-heuristic matching) |
| U3 | P1 | no duplicates | Task-end `ToolCompleted` fallback re-emits every result; idempotency rests on the same fragile text match |
| U4 | P2 | no duplicates | `push_system_message` stacks identical consecutive notices |
| U5 | P2 | no clutter | `[skills] used: …` agent row lands in the transcript after every skill-using turn |
| B1 | P0 | no mangling | `friendly_error` advice strings contain runs of 14+ spaces (3 of 7 branches) |
| B2 | P0 | no mangling | `fail_generation` discards the partial streamed answer and stamps running rows with the raw multi-line error |
| B3 | P0 | no mangling | `Event::System` has no handler — phase hints + corrupt-batch warnings vanish silently |
| B4 | P1 | no mangling | Raw provider errors (multi-line JSON/HTML) render in full; no whitespace collapse, no length cap |
| B5 | P1 | no mangling | `settle_running_tools` writes the raw error into every running tool row body |
| B6 | P2 | no mangling | `wrap_text` breaks words mid-token for user/system/tool rows (markdown path wraps at words) |
| B7 | P2 | proper place | Approval popup height ignores wrapping → key legend clipped on long diffs |
| C1 | P0 | no clutter | Dim `─` rule is emitted between **every** pair of messages, not between turns |
| C2 | P1 | no clutter | Header crams 9 segments; `[theme]` badge and `sandbox:off` are noise; goal is truncated first on narrow terminals |
| C3 | P1 | no clutter | Status line repeats the live tool row brief; `active_tool()` renders `0/0`; no aggregate for N>1 running tools |
| C4 | P1 | less output | Finished tool rows preview 12 body lines by default; the per-row hint is long and repeats on every collapsed row |
| C5 | P2 | no clutter | `(turn took Ns — press any key to focus)` row after every long turn (bell + OSC already notify) |
| C6 | P2 | no duplicates | `⛨ round tainted …` banner re-sent every turn while the taint persists |
| D4 | P2 | code health | Hidden-tool predicate + counting loop duplicated 3× in `ChatWidget::render` |

Execution order = dependency order: **Phase A** (A1–A6: independent one-hunk fixes, all P0s),
**Phase B** (B1–B2: call-identity threading, one commit; also delivers defect C3), **Phase C**
(C1–C4: visual diet + remaining mangling), **Phase D** (D1–D4: one-shot notices and dedupe
guards). Phase A and Phase B are independent of each other; everything in Phase C/D depends on
nothing but the phases before it (noted per task).

**Task → defect mapping (every defect in 0.4 is delivered by exactly one task):**

| Task | Defects delivered |
|---|---|
| A1 | U1 |
| A2 | B1 |
| A3 | B2, B5 |
| A4 | B4 |
| A5 | B3 |
| A6 | C1, D4 |
| B1 | U2 (engine half) |
| B2 | U2 (TUI half), U3, C3 |
| C1 | C2 |
| C2 | C4 |
| C3 | B6 |
| C4 | B7 |
| D1 | U4 |
| D2 | C5 |
| D3 | C6 |
| D4 | U5 |

Beware the two ID namespaces: **task IDs** (A1…D4, the `#### Task X` headings) and **defect
IDs** (U/B/C/D rows in table 0.4). They overlap alphabetically but never numerically map
one-to-one — always go through the table above.

---

## Part 1 — Ground rules for the executing agent

Follow these literally. Do not improvise.

1. **Work in the kod clone only.** All paths below are relative to the repo root
   (`crates/...`). Do not touch `tab-bridge`, the provider crates, or anything under
   `crates/kod-provider*`.
2. **One commit per task**, in the order given. Commit message format:
   `tui(ux): <task id> — <one-line summary>` (e.g. `tui(ux): U1 — remove duplicate tool-error system line`).
   If a task's verification fails, fix forward; do not silently skip the test updates.
3. **Never reformat code you are not editing.** The codebase is `rustfmt`-clean; run
   `cargo fmt --all` at the end of each task and include any formatting fallout in the same commit.
4. **Run the crate's tests after every task:**
   ```bash
   cargo test -p kod-tui
   cargo test -p kod-core --lib engine
   cargo test -p kod-core --test tool_loop
   ```
   A task is done when its own verify command passes **and** the three commands above are green.
5. **`sequence` discipline.** Every message that enters the transcript must go through
   `add_message` (which stamps the monotonic `sequence`). Never push into `self.messages`
   directly. Mutating an existing row in place (as `settle_running_tools` and
   `complete_tool_execution_*` do) is correct and stays correct.
6. **Marker protocol v2 (Phase B) is a same-commit change.** The markers are an in-process wire
   format between `kod-core` and `kod-tui`; ship builder, parser, all consumers, and all tests
   in one commit so no binary ever mixes v1 senders with v2 parsers.
7. **Do not add config knobs** for anything in this plan. The defaults after this plan are the
   product. If you believe a knob is needed, you misread the task.
8. **Comments you copy must be re-wrapped by hand.** Several string literals in this file are
   shown wrapped across source lines; when a literal contains a run of spaces in the *old* code
   block, that is the bug — the *new* code block always shows the intended single-spaced text.
9. **Unicode:** the TUI renders box-drawing (`─ ╭ ╮ ╰ ╯`), icons (`⚙ ✗ ⛨ ⋯ ▸ ❯`), braille
   spinner frames, and CJK via `unicode-width`. Keep them. Do not introduce emoji.
10. **Line numbers are from commit `eb39247`** and drift by a few lines after each task. Match
    on the quoted code, not on the line number.
---

## Part 2 — Tasks

### Phase A — independent one-hunk fixes (P0s first)

Execute in order A1 → A6. No task in this phase depends on another.

---

#### Task A1 (U1) — a tool error is displayed exactly once

**File:** `crates/kod-tui/src/main_loop.rs`

The `Event::ToolCompleted` and `Event::ToolCompletedWithDuration` arms each do two things for an
error result: (1) `complete_tool_execution*` fills the live tool row, which is auto-expanded and
styled `✗` (see `app/tools.rs:124-129` and `163-167`), and (2) pushes an **additional** system
message repeating the same text. Requirement 5 forbids the second one. The tool row is the single
error surface: it stays visible even when `t` hid non-error tools (`ui/chat.rs:437-448` keeps
error rows), and it auto-expands so nothing hides behind the preview.

**Remove** from the `Event::ToolCompleted` arm (currently lines 984-998):

```rust
            Event::ToolCompleted(tool_name, result) => {
                // Tool row first, then whatever streamed during the call:
                // flushing before would drop post-tool text above the row.
                self.app.complete_tool_execution(&tool_name, &result);
                self.app.flush_streamed_text();
                // Failures must be unmissable even if the tool row is collapsed
                // or `t` hid tools — push a red system line as backup.
                if result.trim_start().starts_with("Error:") {
                    self.app.push_system_message(&format!(
                        "Tool `{}` failed: {}",
                        tool_name,
                        result.trim()
                    ));
                }
            }
```

**Replace with:**

```rust
            Event::ToolCompleted(tool_name, result) => {
                // Tool row first, then whatever streamed during the call:
                // flushing before would drop post-tool text above the row.
                self.app.complete_tool_execution(&tool_name, &result);
                self.app.flush_streamed_text();
                // Errors are NOT repeated as a system message here. The tool
                // row is the single error surface: auto-expanded on failure,
                // styled `✗`, and never hidden by the `t` toggle
                // (`ChatWidget::render` keeps error rows visible). A second
                // copy in the transcript is noise, not redundancy.
            }
```

Apply the same removal inside the `Event::ToolCompletedWithDuration` arm (delete only the
`if result.trim_start().starts_with("Error:") { … }` block; keep the
`complete_tool_execution_with_duration` + `flush_streamed_text` calls).

**Tests:** grep the crate for assertions on the removed line —
`rg -n 'failed:' crates/kod-tui` — and update every test that asserted the system duplicate
(inline tests around `main_loop.rs:6939-6999` assert tool-row content; any assertion of the form
`contains("Tool `")` on *system* rows must be deleted or retargeted at the tool row). Add this
regression test next to them:

```rust
    #[tokio::test]
    async fn tool_error_is_displayed_once_not_duplicated_as_a_system_line() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::ToolStarted("execute_command".to_string())).await;
        tui.handle_event(Event::ToolCompleted(
            "execute_command cargo check".to_string(),
            "Error: exit status 1".to_string(),
        ))
        .await;
        let rows_with_error = tui
            .app()
            .messages()
            .iter()
            .filter(|m| m.content.contains("exit status 1"))
            .count();
        assert_eq!(rows_with_error, 1, "one row carries the error, not two");
        assert!(
            tui.app()
                .messages()
                .iter()
                .all(|m| !m.content.starts_with("Tool `execute_command` failed")),
            "the legacy duplicate system line must be gone",
        );
    }
```

(Match the exact construction pattern of the neighboring tests — `test_tui()` here stands for
whatever helper the surrounding module uses to build a `TuiLoop` with a no-op engine.)

**Verify:** `cargo test -p kod-tui tool_error_is_displayed_once`

---

#### Task A2 (B1) — rewrite the mangled error-advice strings

**File:** `crates/kod-tui/src/app/streaming.rs`, `friendly_error` (lines 212-270).

Three advice literals contain runs of 14+ internal spaces — source-line-wrap artifacts that
render as giant gaps in the chat. Replace the entire `let advice = if … ;` chain with the block
below (match arms and semantics unchanged; only the literals are rewritten, plus the final
composition moves to Task A4 — keep the existing `format!("Error: {error}")` +
`out.push_str(advice)` composition for now):

```rust
        let advice = if lower.contains("model not found")
            || lower.contains("model `")
            || lower.contains("unknown model")
            || lower.contains("no such model")
            || lower.contains("model does not exist")
        {
            // The most common first-run mistake: config or `/model <name>`
            // names a model the server has never pulled. The fix is one
            // command, so name it.
            " The model named in the config or `/model` is not on the server. Pull it first (e.g. `ollama pull codellama:13b`), or run `/model <name>` with a model the server already has."
        } else if lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("too many tokens")
            || lower.contains("maximum context")
            || lower.contains("exceeds the maximum")
        {
            " The prompt exceeded the model's context window. Run `/compact` to trim the session, or start a fresh chat with `/clear`."
        } else if (lower.contains("json") && lower.contains("parse"))
            || lower.contains("invalid tool")
            || lower.contains("malformed function")
            || lower.contains("tool_call")
        {
            " The model returned a tool call that could not be parsed. Retrying usually helps — if it persists, the model may not support tool calling at all (try a larger or newer model, or a codellama/qwen2.5-coder build)."
        } else if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("failed to connect")
            || lower.contains("connection closed")
        {
            " Could not reach the model server — is it running? For Ollama: `ollama serve`, then check `base_url` in the kod config."
        } else if lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("api key")
        {
            " Looks like an auth problem — check `api_key` in the kod config."
        } else if lower.contains("404") {
            " Endpoint not found — check `base_url` ends with `/v1` for OpenAI-compatible servers."
        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure).
            ""
        };
```

**Add** this test inside `app/tests.rs` (or the file's existing test module for streaming):

```rust
    #[test]
    fn friendly_error_advice_never_contains_double_spaces() {
        let samples = [
            "model `qwen99` not found",
            "this prompt exceeds the maximum context length",
            "invalid tool_call: json parse failed",
            "connection refused (os error 111)",
            "401 unauthorized",
            "404 not found",
            "request timed out",
            "something entirely unknown",
        ];
        for s in samples {
            let out = KodApp::friendly_error(s, 0);
            assert!(
                !out.contains("  "),
                "advice for {s:?} carries a space run: {out:?}"
            );
        }
    }
```

**Verify:** `cargo test -p kod-tui friendly_error`

---

#### Task A3 (B2 + B5) — a failed turn keeps its partial answer and stops stamping raw errors into rows

**File:** `crates/kod-tui/src/app/streaming.rs`

Two defects in `fail_generation` (lines 185-203):

1. The text the user watched stream is silently discarded (`self.current_response.clear()` with
   no flush). `cancel_generation` (lines 275-302) keeps it as `(cancelled — partial answer)`.
   A failure must behave the same — watching content vanish is the "mangled" experience.
2. `settle_running_tools(ToolStatus::Failed, error)` writes the **raw, possibly multi-line
   provider error** into every running tool row's body (`settle_running_tools`, lines 160-181),
   where it sits next to the friendly error pushed a few lines later — duplicated and mangled.

**Remove** the whole current `fail_generation` and **insert:**

```rust
    /// Record a generation failure: keep the partial answer, settle the
    /// running tool rows, and surface one actionable error message.
    pub fn fail_generation(&mut self, error: &str) {
        // Same rationale as cancel_generation: settle the flag so the
        // next turn does not inherit "real usage seen" from this one.
        self.turn_has_real_usage = false;
        // Keep whatever streamed before the failure — the same contract
        // cancel_generation implements. Watching half a reply vanish is
        // worse than seeing it marked partial.
        let partial = Self::trim_blank_lines(&self.current_response);
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.first_chunk_at = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count += 1;
        // Running rows get a one-line pointer, not the raw error: the
        // friendly message below is the single detailed source.
        self.settle_running_tools(ToolStatus::Failed, "failed — see the error below");
        if !partial.is_empty() {
            self.note_usage(partial.len());
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::Assistant,
                content: format!("{partial}\n(error — partial answer)"),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            });
            self.stream_flushed_bubble = true;
        }
        self.last_error = Some(error.to_string());
        self.push_system_message(&Self::friendly_error(error, self.fail_count));
        // Errors live in the chat scroll view — never in a strip above
        // the input. Pin the viewport so the error is actually visible
        // even when the user had scrolled up reading history.
        self.scroll_to_bottom();
    }
```

**Add** regression tests:

```rust
    #[test]
    fn fail_generation_keeps_the_partial_streamed_answer() {
        let mut app = KodApp::new();
        app.begin_generation();
        app.start_response_stream();
        app.add_response_chunk("half a repl");
        app.fail_generation("connection reset");
        let texts: Vec<&str> = app
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("half a repl")
                && t.contains("(error — partial answer)")),
            "partial answer must survive the failure: {texts:?}"
        );
    }

    #[test]
    fn fail_generation_never_copies_the_raw_error_into_tool_rows() {
        let mut app = KodApp::new();
        app.begin_generation();
        app.start_tool_execution("execute_command");
        app.fail_generation("HTTP 500 {\"error\":{\"message\":\"boom\"}}");
        let raw_in_tool_row = app.messages().iter().any(|m| {
            m.role == MessageRole::Tool && m.content.contains("HTTP 500")
        });
        assert!(!raw_in_tool_row, "tool rows carry the pointer note, not the dump");
    }
```

**Verify:** `cargo test -p kod-tui fail_generation`

---

#### Task A4 (B4) — collapse and cap the error text the chat shows

**File:** `crates/kod-tui/src/app/streaming.rs`, `friendly_error` composition (end of the function).

Today: `let mut out = format!("Error: {error}"); out.push_str(advice); …`. A provider error that
is a multi-line JSON/HTML dump renders as confetti inside the chat bubble, and the advice glues
straight onto the last raw line. The full text is still available in `last_error` (kept by
`fail_generation`) and in the session log.

**Replace** the tail of `friendly_error` (from `let mut out = …` to the closing `out`) with:

```rust
        // Single-line summary of the raw error: provider errors often
        // carry multi-line JSON/HTML bodies that render as confetti in
        // the chat. Collapse horizontal whitespace (keep newlines out
        // entirely) and cap the length; the full text stays in
        // `last_error` and the session log.
        let mut summary: String = error.split_whitespace().collect::<Vec<_>>().join(" ");
        if summary.chars().count() > 240 {
            summary = summary.chars().take(240).collect::<String>();
            summary.push_str(" …");
        }
        let mut out = format!("Error: {summary}");
        if !advice.is_empty() {
            out.push('\n');
            out.push_str(advice.trim());
        }
        if fail_count >= 2 {
            out.push_str(
                " (offline mode: generation keeps failing — fix the server, then `/retry`)",
            );
        }
        out
```

**Extend** the A2 test with one more case:

```rust
    #[test]
    fn friendly_error_collapses_multiline_provider_dumps() {
        let raw = "HTTP 500\n{\n  \"error\": {\n    \"message\": \"boom\"\n  }\n}";
        let out = KodApp::friendly_error(raw, 0);
        assert_eq!(out.lines().next().unwrap(), "Error: HTTP 500 { \"error\": { \"message\": \"boom\" } }");
        assert_eq!(out.lines().count(), 1, "no advice branch → single line");
    }
```

**Verify:** `cargo test -p kod-tui friendly_error`

---

#### Task A5 (B3) — handle `Event::System` (stop dropping engine notices)

**File:** `crates/kod-tui/src/main_loop.rs`, `handle_event`.

`Event::System(EventPriority, String)` is sent from two places (phase-change hint at
`main_loop.rs:850-856`; corrupt-approval-batch warning at `main_loop.rs:1323-1333`) and handled
by **no arm** — it falls into `_ => {}` (line 1107). The notices vanish.

**Insert** this arm into the `match event { … }` (anywhere among the arms, above `_ => {}`):

```rust
            Event::System(_priority, msg) => {
                // Display-only notices from the engine (phase-change
                // hints, approval-parse failures). They were sent into
                // the event queue and silently dropped before — every
                // System event must land in the transcript as a `sys`
                // row. Priority already ordered them in the queue
                // (`EventHandler`), it carries no extra semantics here.
                self.app.push_system_message(&msg);
            }
```

**Add** a test next to the other `handle_event` tests:

```rust
    #[tokio::test]
    async fn system_events_reach_the_transcript_instead_of_vanishing() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::System(
            crate::event::EventPriority::Normal,
            "(phase changed: setup → implementation. Consider /handoff.)".to_string(),
        ))
        .await;
        assert!(tui
            .app()
            .messages()
            .iter()
            .any(|m| m.content.contains("phase changed: setup → implementation")));
    }
```

**Verify:** `cargo test -p kod-tui system_events_reach_the_transcript`

---

#### Task A6 (C1 + D4) — the dim rule separates turns, not messages; one hidden-tool predicate

**File:** `crates/kod-tui/src/ui/chat.rs`, `ChatWidget::render` (lines 343-465).

Today the final pass pushes a `─` rule before **every** message after the first
(lines 449-454), so a turn reads:

```
you
─────
⚙ read_file …
─────
╭─ ai ─╮
─────
sys (turn took 3s)
```

The comment above it claims "a dim rule between turns". Make the code match the comment.

**Step 1 — add one predicate** near the top of the file (below `TOOL_DISPLAY_LINES`):

```rust
impl ChatWidget {
    /// True when this message is skipped because tools are hidden
    /// (`t` toggle). Errors are never hidden — a collapsed `t` must
    /// not bury failures. Single source for the probe pass, the
    /// hidden-count, and the final render loop.
    fn tool_row_hidden(app: &KodApp, m: &Message) -> bool {
        if app.search_query().is_some() || app.show_tools() || m.role != MessageRole::Tool {
            return false;
        }
        let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("");
        !body.trim_start().starts_with("Error:")
    }

    /// True when a dim rule is drawn above this message: a rule marks
    /// where a user turn begins. Everything inside a turn (tool rows,
    /// assistant bubbles, system notes) flows unseparated.
    fn starts_new_turn(app: &KodApp, m: &Message, is_first: bool) -> bool {
        let _ = app;
        !is_first && m.role == MessageRole::User
    }
}
```

**Step 2 — probe pass.** Replace the two skip sites and the rule accounting in the probe loop
(lines 362-400):

```rust
            let mut hidden = 0;
            for m in &ordered {
                if Self::tool_row_hidden(app, m) {
                    hidden += 1;
                }
            }
```

and inside the probe `for` loop:

```rust
            for (i, m) in ordered.iter().enumerate() {
                if Self::tool_row_hidden(app, m) {
                    continue;
                }
                let rule_rows =
                    usize::from(Self::starts_new_turn(app, m, i == 0) && !prev_tail_blank);
                probe_rows += rule_rows;
                let (rows, tail_blank) = Self::message_measurement_cached(app, m, narrow_width);
                probe_rows += rows;
                prev_tail_blank = tail_blank;
                probe_rendered = true;
            }
```

**Step 3 — final pass.** Replace lines 436-457 with:

```rust
        for (i, message) in ordered.iter().enumerate() {
            if Self::tool_row_hidden(app, message) {
                hidden_tools += 1;
                continue;
            }
            if Self::starts_new_turn(app, message, i == 0)
                && lines.last().map(|l| l.width()).unwrap_or(1) > 0
            {
                lines.push(Line::from(vec![Span::styled(
                    "─".repeat(text_width.min(120)),
                    Style::default().fg(theme.dim),
                )]));
            }
            message_line_offsets.insert(message.id.clone(), lines.len());
            lines.extend(Self::message_lines(app, message, text_width));
        }
```

(Delete the now-unused per-loop `body`/`is_error` extraction that fed the old hidden check.)

**Tests to update/add** (chat.rs coverage module and `tests/ui.rs`):

```rust
    #[test]
    fn render_draws_the_rule_only_before_user_turns() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::User, "first question");
        push_message(&mut app, MessageRole::Tool, "[read_file] path=main.rs\nok");
        push_message(&mut app, MessageRole::Assistant, "the answer");
        push_message(&mut app, MessageRole::System, "a note");
        push_message(&mut app, MessageRole::User, "second question");
        let text = render(&app, 100, 60);
        // 1 rule: above "second question" only.
        let rules = text.matches('─').count();
        assert!(
            rules >= 1,
            "the turn boundary must be visible: {text}"
        );
        let between = text
            .split("first question")
            .nth(1)
            .unwrap()
            .split("second question")
            .next()
            .unwrap()
            .to_string();
        assert!(
            !between.contains('\u{2500}'),
            "no rule inside the turn (tool/assistant/sys rows): {between:?}"
        );
    }
```

Run the full `render_tool_row_with_show_tools_off_hides_non_error_rows` and
`render_error_tool_remains_visible_when_show_tools_is_off` tests — they must still pass
unchanged (`tool_row_hidden` preserves their semantics).

**Verify:** `cargo test -p kod-tui render_draws_the_rule_only_before_user_turns && cargo test -p kod-tui show_tools`

---

### Phase B — call identity: everything at its proper place, nothing duplicated

Phase B is **one commit** (ground rule 6). It contains two tasks that must land together:
`B1` (engine markers v2) and `B2` (TUI row registry + events). Execute B1 fully, then B2, then
run the full test suite before committing.

---

#### Task B1 (U2, engine side) — marker protocol v2: every marker carries the call id

**File:** `crates/kod-core/src/engine/mod.rs`

The three builders/parsers at lines 116-178 gain a **call id** parameter/field. Call id = the
provider's tool-call id (`ToolCall.id: Option<String>`), or the empty string `""` when the
provider does not send one (`""` selects the legacy heuristic path in the TUI). Ids must not
contain `:` or `\0` — sanitize defensively.

**Replace** the three marker sections (lines 116-178) with:

```rust
/// Marker prefix for tool-start notices inside the `process_streaming`
/// chunk channel: `\0kod-tool:<cid>:<name>\0`. `<cid>` is the provider's
/// tool-call id (or `""` when the provider sends none) — the TUI keys its
/// live tool-row registry on it so parallel calls update their own rows
/// (see `parse_tool_start`).
pub const TOOL_START_MARKER: &str = "\0kod-tool:";

/// Sanitize a call id for the marker wire format: no `:` (field
/// separator) and no `\0` (marker terminator) may survive.
fn clean_call_id(cid: &str) -> String {
    cid.replace([':', '\0'], "_")
}

/// Build a tool-start marker chunk for call `cid` carrying `name`.
pub fn tool_start_marker(cid: &str, name: &str) -> String {
    format!("{TOOL_START_MARKER}{}:{name}\0", clean_call_id(cid))
}

/// If `chunk` is a tool-start marker, return `(call_id, tool_name)`.
/// A legacy v1 chunk (no `cid:` prefix) parses as `("", name)`.
pub fn parse_tool_start(chunk: &str) -> Option<(&str, &str)> {
    let rest = chunk.strip_prefix(TOOL_START_MARKER)?.strip_suffix('\0')?;
    Some(rest.split_once(':').unwrap_or(("", rest)))
}

/// Marker prefix for tool-argument excerpts inside the
/// `process_streaming` chunk channel: `\0kod-args:<cid>:<display>\0`.
pub const TOOL_ARGS_MARKER: &str = "\0kod-args:";

/// Build a tool-args marker chunk carrying a one-line display string.
pub fn tool_args_marker(cid: &str, display: &str) -> String {
    format!("{TOOL_ARGS_MARKER}{}:{display}\0", clean_call_id(cid))
}

/// If `chunk` is a tool-args marker, return `(call_id, display)`.
pub fn parse_tool_args(chunk: &str) -> Option<(&str, &str)> {
    let rest = chunk.strip_prefix(TOOL_ARGS_MARKER)?.strip_suffix('\0')?;
    Some(rest.split_once(':').unwrap_or(("", rest)))
}

/// Marker prefix for per-tool completion notices inside the
/// `process_streaming` chunk channel:
/// `\0kod-done:<cid>\0<header>\0<summary>\0<duration_ms>`.
pub const TOOL_DONE_MARKER: &str = "\0kod-done:";

/// Build a tool-done marker chunk for one finished call. Headers and
/// summaries are sanitized (no `\0`) when built here; the call id is
/// sanitized too (`:` / `\0` cannot survive the wire format).
pub fn tool_done_marker(cid: &str, header: &str, summary: &str, duration_ms: u64) -> String {
    let clean = |s: &str| s.replace('\0', " ");
    format!(
        "{TOOL_DONE_MARKER}{}\0{}\0{}\0{duration_ms}",
        clean_call_id(cid),
        clean(header),
        clean(summary)
    )
}

/// If `chunk` is a tool-done marker, return `(call_id, header, summary,
/// duration_ms)`. A missing or malformed duration degrades to `0`
/// rather than dropping the completion.
pub fn parse_tool_done(chunk: &str) -> Option<(&str, &str, &str, u64)> {
    let rest = chunk.strip_prefix(TOOL_DONE_MARKER)?;
    let mut parts = rest.splitn(4, '\0');
    let cid = parts.next()?;
    let header = parts.next()?;
    let summary = parts.next()?;
    let duration = parts.next().unwrap_or("0");
    Some((cid, header, summary, duration.parse::<u64>().unwrap_or(0)))
}
```

**Update the three emission sites in the same file:**

1. Start marker, inside `StreamChunk::ToolCallStart` handling (line ~10184-10192). The `id`
   binding is the provider call id the chunk carries:

```rust
                StreamChunk::ToolCallStart { index, id, name } => {
                    let entry = partials.entry(index).or_default();
                    if entry.id.is_none() {
                        entry.id = id;
                    }
                    if entry.name.is_none() {
                        entry.name = Some(name.clone());
                        let cid = entry.id.as_deref().unwrap_or("");
                        let _ = chunk_tx.send(tool_start_marker(cid, &name)).await;
                    }
                }
```

2. Args markers, after the round's calls are assembled (line ~9810-9819):

```rust
            for call in &calls {
                let cid = call.id.as_deref().unwrap_or("");
                let _ = chunk_tx
                    .send(tool_args_marker(cid, &format_call_brief(
                        &call.tool_name,
                        &call.arguments,
                    )))
                    .await;
            }
```

3. Done markers, per finished call (line ~9860-9874):

```rust
            for (call, (result, ms)) in calls
                .iter()
                .zip(section.results.iter().zip(section.elapsed_ms.iter()))
            {
                let cid = call.id.as_deref().unwrap_or("");
                let header = format_tool_header(&call.tool_name, &call.arguments);
                let summary = summarize_tool_result(&call.tool_name, result);
                let _ = chunk_tx
                    .send(tool_done_marker(cid, &header, &summary, *ms))
                    .await;
            }
```

**Update the destructuring consumers (same commit):**

- `crates/kod-core/src/acp.rs:562` — `if let Some(name) = crate::engine::parse_tool_start(chunk)`
  becomes:

```rust
    if let Some((_cid, name)) = crate::engine::parse_tool_start(chunk) {
```

- `crates/kod-core/src/acp.rs:612` — `if let Some((header, summary, _ms)) = …parse_tool_done(chunk)`
  becomes:

```rust
    if let Some((_cid, header, summary, _ms)) = crate::engine::parse_tool_done(chunk) {
```

- `kod-cli/src/commands/chat.rs:515`, `kod-cli/src/commands/prompt.rs:195`,
  `kod-core/src/swarm_runner.rs:1234-1241` use `is_some()` only — **no change**.

**Update the engine tests** in `crates/kod-core/src/engine/mod.rs` (test module, lines ~14576-14602
and the wire-protocol block ~15410-15500) and `crates/kod-core/tests/tool_loop.rs` (lines 113-170,
395-430). The builder call sites gain the id argument and every parse destructure gains the id
element. Exact new assertions for the roundtrip block:

```rust
    #[test]
    fn test_tool_done_marker_roundtrip() {
        let header = String::from("read_file path=main.rs");
        let summary = String::from("12 lines");
        let chunk = tool_done_marker("call_1", &header, &summary, 1340);
        let (cid, h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!((cid, h, s, ms), ("call_1", "read_file path=main.rs", "12 lines", 1340));
    }

    #[test]
    fn test_tool_done_marker_sanitizes_nul() {
        let chunk = tool_done_marker("c", "a\0b", "c\0d", 7);
        let (cid, h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!((cid, h, s, ms), ("c", "a b", "c d", 7));
    }

    #[test]
    fn test_tool_done_marker_rejects_other_chunks() {
        assert!(parse_tool_done("plain text").is_none());
        assert!(parse_tool_done(&tool_start_marker("read_file", "read_file")).is_none());
        // Missing fields: a 3-field legacy body is cid+header+summary with a
        // garbage duration -> cid must NOT swallow the header.
        let raw = format!("{TOOL_DONE_MARKER}h\0s\0abc");
        assert_eq!(parse_tool_done(&raw), Some(("h", "s", "abc", 0)));
        let raw = format!("{TOOL_DONE_MARKER}only-header");
        assert!(parse_tool_done(&raw).is_none());
    }

    #[test]
    fn test_marker_v2_roundtrip_with_call_ids() {
        let start = tool_start_marker("call_9", "read_file");
        assert_eq!(parse_tool_start(&start), Some(("call_9", "read_file")));
        // Legacy v1 chunk (no cid) still parses with an empty id.
        assert_eq!(parse_tool_start("\0kod-tool:read_file\0"), Some(("", "read_file")));
        let args = tool_args_marker("call_9", "execute_command cargo test -- --foo:bar");
        assert_eq!(
            parse_tool_args(&args),
            Some(("call_9", "execute_command cargo test -- --foo:bar")),
            "the first ':' separates the id; colons inside the display survive"
        );
        assert_eq!(parse_tool_args(""), None);
    }

    #[test]
    fn test_call_id_colons_are_sanitized() {
        let start = tool_start_marker("we:ird\0id", "read_file");
        let (cid, name) = parse_tool_start(&start).expect("must parse");
        assert_eq!(name, "read_file");
        assert!(!cid.contains(':') && !cid.contains('\0'));
    }
```

In `tool_loop.rs`, update the two destructures:

```rust
        if let Some((cid, name)) = parse_tool_start(&chunk) {
            // cid may be "" in fixtures that do not model provider ids.
            let _ = cid;
```

and the import line gains nothing (same function names).

**Verify:**

```bash
cargo test -p kod-core --lib engine::tests::test_tool_done
cargo test -p kod-core --lib engine::tests::test_marker_v2
cargo test -p kod-core --test tool_loop
cargo build --workspace   # acp.rs and the CLIs must compile
```

---

#### Task B2 (U2 + U3 + C3, TUI side) — key tool rows by call id; task-end fallback becomes idempotent

**Files:** `crates/kod-tui/src/event.rs`, `crates/kod-tui/src/app/mod.rs`,
`crates/kod-tui/src/app/tools.rs`, `crates/kod-tui/src/app/ui_state.rs`,
`crates/kod-tui/src/main_loop.rs`, `crates/kod-tui/src/ui/status.rs`, plus tests.

**Step 1 — events gain the call id.** In `event.rs` replace the four variants (lines 99-108):

```rust
    ToolStarted {
        /// Provider tool-call id; `""` when unknown (legacy heuristic path).
        id: String,
        name: String,
    },
    ToolCompleted {
        id: String,
        header: String,
        summary: String,
    },
    /// Live per-tool completion from the engine's done-marker: same row
    /// fill as `ToolCompleted`, plus wall time for the header
    /// (`execute_command … · 1.2s`).
    ToolCompletedWithDuration {
        id: String,
        header: String,
        summary: String,
        duration_ms: u64,
    },
    /// Live one-line excerpt of what the running tool is doing.
    ToolProgress {
        id: String,
        display: String,
    },
```

**Step 2 — registry on `KodApp`.** In `app/mod.rs`, add two fields to the struct (near
`tool_executions`) and initialize them in `KodApp::new()`:

```rust
    /// Live tool rows keyed by provider call id. Written by
    /// `start_tool_execution`, consumed (and removed) by completion/failure.
    /// An id of `""` never enters this map — those calls use the legacy
    /// text-heuristic path.
    tool_rows_by_call: HashMap<String, MessageId>,
    /// Call ids whose rows are filled. The task-end `ToolCompleted`
    /// fallback for such an id is a no-op (U3: no duplicate rows).
    completed_calls: HashSet<String>,
```

```rust
            tool_rows_by_call: HashMap::new(),
            completed_calls: HashSet::new(),
```

Add accessors used by the status bar (D2):

```rust
    /// Number of tool rows still showing the live "running…" placeholder.
    pub fn running_tool_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.content
                        .split_once('\n')
                        .map(|x| x.1)
                        .unwrap_or("")
                        .trim()
                        == Self::LIVE_TOOL_BODY_PLACEHOLDER
            })
            .count()
    }
```

**Step 3 — rewrite `app/tools.rs`** so every lifecycle method takes the id and targets the
registered row first. Replace the file's method bodies as follows (keep the file header comment;
`use super::*;` already brings `HashMap`/`HashSet` via `app/mod.rs`):

```rust
impl KodApp {
    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
    }

    pub fn tool_executions(&self) -> &[ToolExecution] {
        &self.tool_executions
    }

    pub fn start_tool_execution(&mut self, id: &str, tool_name: &str) {
        // Repeat start for a call we already finished (duplicated marker,
        // engine round restart): never spawn a second row, and never
        // resurrect "running…" phase state for a settled call.
        if !id.is_empty() {
            if self.completed_calls.contains(id) {
                return;
            }
            if let Some(msg_id) = self.tool_rows_by_call.get(id).cloned() {
                self.refresh_tool_row_header(&msg_id, tool_name);
                self.current_tool = Some(tool_name.to_string());
                self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));
                return;
            }
        }

        self.current_tool = Some(tool_name.to_string());
        self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));

        self.tool_executions.push(ToolExecution {
            tool_name: tool_name.to_string(),
            status: ToolStatus::Running,
            start_time: Utc::now(),
            result: None,
        });
        // Stream the row live: it lands in position now with a running
        // body, and completion fills that same row in — the call never
        // arrives as a block at task end.
        let msg = Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{tool_name}]\n{}", Self::LIVE_TOOL_BODY_PLACEHOLDER),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        };
        let msg_id = msg.id.clone();
        if !id.is_empty() {
            self.tool_rows_by_call.insert(id.to_string(), msg_id.clone());
        }
        self.add_message(msg);
    }

    /// Rewrite one registered row's header in place, keeping its body.
    fn refresh_tool_row_header(&mut self, msg_id: &MessageId, header: &str) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == *msg_id) {
            let body = m
                .content
                .split_once('\n')
                .map(|x| x.1.to_string())
                .unwrap_or_else(|| Self::LIVE_TOOL_BODY_PLACEHOLDER.to_string());
            m.content = format!("[{header}]\n{body}");
        }
    }

    /// Index of the most recent tool row still awaiting its result
    /// (legacy path for calls whose provider sends no ids).
    fn live_tool_msg(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("").trim();
            body == Self::LIVE_TOOL_BODY_PLACEHOLDER
        })
    }

    /// Resolve the row a completion targets: the registered call row
    /// first, then the newest live placeholder, then (legacy, empty-id
    /// only) the header-text heuristics. Returns the `MessageId` and
    /// whether the row was found.
    fn resolve_tool_row(&mut self, id: &str, tool_name: &str) -> Option<MessageId> {
        if !id.is_empty() {
            if let Some(msg_id) = self.tool_rows_by_call.remove(id) {
                return Some(msg_id);
            }
        }
        if let Some(i) = self.live_tool_msg() {
            return Some(self.messages[i].id.clone());
        }
        if id.is_empty() {
            // Legacy header-prefix match (the pre-v2 behavior), kept only
            // for providers that send no call ids.
            if let Some(i) = self.messages.iter().rposition(|m| {
                m.role == MessageRole::Tool && m.content.starts_with(&format!("[{tool_name}]"))
            }) {
                return Some(self.messages[i].id.clone());
            }
        }
        None
    }

    /// Refresh the live "running …" line with a one-line excerpt of what
    /// the tool is actually doing (`execute_command cargo test …`).
    pub fn update_tool_status(&mut self, id: &str, display: &str) {
        let display = display.trim();
        if display.is_empty() {
            return;
        }
        self.current_tool = Some(display.to_string());
        self.set_phase(GenPhase::ExecutingTool(display.to_string()));
        if let Some(msg_id) = self.tool_rows_by_call.get(id).cloned() {
            self.refresh_tool_row_header(&msg_id, display);
        } else if let Some(i) = self.live_tool_msg() {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or(Self::LIVE_TOOL_BODY_PLACEHOLDER);
            let body = body.to_string();
            self.messages[i].content = format!("[{display}]\n{body}");
        }
    }

    pub fn complete_tool_execution(&mut self, id: &str, tool_name: &str, result: &str) {
        self.complete_tool_execution_with_duration(id, tool_name, result, None);
    }

    /// Fill the addressed tool row with its result, stamping the header
    /// with wall time when `duration_ms` is present (`header · 1.2s`).
    ///
    /// Idempotent by call id: the task-end fallback for a call the live
    /// done-marker already completed is a no-op (U3).
    pub fn complete_tool_execution_with_duration(
        &mut self,
        id: &str,
        tool_name: &str,
        result: &str,
        duration_ms: Option<u64>,
    ) {
        if !id.is_empty() && self.completed_calls.contains(id) {
            return; // task-end fallback after the live done-marker
        }

        let msg_id = self.resolve_tool_row(id, tool_name);
        self.current_tool = None;
        self.set_phase(GenPhase::Summarizing);

        let body = Self::trim_blank_lines(result);
        let is_error = body.trim_start().starts_with("Error:");
        let header = match duration_ms {
            Some(ms) => format!("{tool_name} · {}", kod_core::engine::format_duration_ms(ms)),
            None => tool_name.to_string(),
        };
        let content = format!("[{header}]\n{body}");

        let msg_id = match msg_id {
            Some(msg_id) => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.content = content;
                }
                msg_id
            }
            None => {
                let msg = Message {
                    id: MessageId::new(),
                    role: MessageRole::Tool,
                    content,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                };
                let msg_id = msg.id.clone();
                self.add_message(msg);
                msg_id
            }
        };

        if !id.is_empty() {
            self.tool_rows_by_call.remove(id);
            self.completed_calls.insert(id.to_string());
        }
        // Mark the ToolExecution ledger entry settled.
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.status == ToolStatus::Running
                && (e.tool_name == tool_name || tool_name.starts_with(&format!("{} ", e.tool_name)))
        }) {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
        }
        // Errors must be unmistakable: auto-expand so the full message is
        // visible and never hidden behind the preview cap.
        if is_error {
            self.expanded_tools.insert(msg_id);
        }
    }

    pub fn fail_tool_execution(&mut self, id: &str, tool_name: &str, error: &str) {
        if !id.is_empty() && self.completed_calls.contains(id) {
            return;
        }
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.status == ToolStatus::Running
                && (e.tool_name == tool_name || tool_name.starts_with(&format!("{} ", e.tool_name)))
        }) {
            execution.status = ToolStatus::Failed;
            execution.result = Some(error.to_string());
        }
        self.current_tool = None;

        let body = format!("Error: {}", error.trim());
        let content = format!("[{tool_name}]\n{body}");
        let msg_id = match self.resolve_tool_row(id, tool_name) {
            Some(msg_id) => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.content = content;
                }
                msg_id
            }
            None => {
                let msg = Message {
                    id: MessageId::new(),
                    role: MessageRole::Tool,
                    content,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                };
                let msg_id = msg.id.clone();
                self.add_message(msg);
                msg_id
            }
        };
        if !id.is_empty() {
            self.tool_rows_by_call.remove(id);
            self.completed_calls.insert(id.to_string());
        }
        // Always expand errors — same rationale as complete_tool_execution.
        self.expanded_tools.insert(msg_id);
    }

    pub fn toggle_tool_expanded(&mut self, id: &MessageId) -> bool {
        if self.expanded_tools.remove(id) {
            false
        } else {
            self.expanded_tools.insert(id.clone());
            true
        }
    }

    pub fn is_tool_expanded(&self, id: &MessageId) -> bool {
        self.expanded_tools.contains(id)
    }

    pub fn show_tools(&self) -> bool {
        self.show_tools
    }

    pub fn toggle_show_tools(&mut self) -> bool {
        self.show_tools = !self.show_tools;
        self.show_tools
    }
}
```

Delete `tool_row_completed_like` — its job is done by `completed_calls`.

**Step 4 — `settle_running_tools`** (in `app/streaming.rs`, lines 160-181) must also drain the
registry so a later fallback for those calls cannot fill stale rows:

```rust
    fn settle_running_tools(&mut self, status: ToolStatus, note: &str) {
        for execution in self.tool_executions.iter_mut() {
            if execution.status == ToolStatus::Running {
                execution.status = status;
                execution.result = Some(note.to_string());
            }
        }
        for m in self.messages.iter_mut() {
            if m.role != MessageRole::Tool {
                continue;
            }
            let mut parts = m.content.splitn(2, '\n');
            parts.next();
            if parts.next().unwrap_or("").trim() == Self::LIVE_TOOL_BODY_PLACEHOLDER {
                let header = m.content.lines().next().unwrap_or("").to_string();
                m.content = format!("{header}\n{note}");
            }
        }
        // Those rows will never receive a completion: forget the ids so
        // a stray fallback cannot resurrect them.
        self.tool_rows_by_call.clear();
        self.current_tool = None;
    }
```

**Step 5 — `main_loop.rs` arms and pump.** Update the four arms (lines ~875-884, 984-1016):

```rust
            Event::ToolStarted { id, name } => {
                // Flush text streamed so far as its own bubble first: the
                // reply before the call belongs above the tool row, the
                // reply after it below — never one giant bubble.
                self.app.flush_streamed_text();
                self.app.start_tool_execution(&id, &name);
            }
            Event::ToolProgress { id, display } => {
                self.app.update_tool_status(&id, &display);
            }
```

```rust
            Event::ToolCompleted { id, header, summary } => {
                // Tool row first, then whatever streamed during the call:
                // flushing before would drop post-tool text above the row.
                self.app.complete_tool_execution(&id, &header, &summary);
                self.app.flush_streamed_text();
                // No duplicate system line for errors: the tool row is the
                // single error surface (Task A1).
            }
            Event::ToolCompletedWithDuration { id, header, summary, duration_ms } => {
                self.app
                    .complete_tool_execution_with_duration(&id, &header, &summary, Some(duration_ms));
                self.app.flush_streamed_text();
            }
```

Update the pump's parse arms (lines ~1355-1372):

```rust
                    } else if let Some((cid, tool)) = kod_core::engine::parse_tool_start(&chunk) {
                        let _ = event_tx_chunks
                            .send(Event::ToolStarted { id: cid.to_string(), name: tool.to_string() })
                            .await;
                    } else if let Some((cid, progress)) = kod_core::engine::parse_tool_args(&chunk) {
                        let _ = event_tx_chunks
                            .send(Event::ToolProgress {
                                id: cid.to_string(),
                                display: progress.to_string(),
                            })
                            .await;
                    } else if let Some((cid, header, summary, duration)) =
                        kod_core::engine::parse_tool_done(&chunk)
                    {
                        let _ = event_tx_chunks
                            .send(Event::ToolCompletedWithDuration {
                                id: cid.to_string(),
                                header: header.to_string(),
                                summary: summary.to_string(),
                                duration_ms: duration,
                            })
                            .await;
                    }
```

Update the task-end fallback (lines ~1451-1466) to pass ids — this is what makes U3 exact. Note
the `None => format!("[{name}]")` branch also loses its doubled brackets (the old code wrapped
`name` in `[]` and `complete_tool_execution` wrapped it again):

```rust
                    let calls: Vec<_> = response.tool_calls.iter().collect();
                    for (i, result) in response.tool_results.iter().enumerate() {
                        let call = calls.get(i).copied();
                        let name = call.map(|c| c.tool_name.as_str()).unwrap_or("tool");
                        let id = call.and_then(|c| c.id.clone()).unwrap_or_default();
                        let header = match call {
                            Some(c) => {
                                kod_core::engine::format_tool_header(&c.tool_name, &c.arguments)
                            }
                            None => name.to_string(),
                        };
                        let summary = kod_core::engine::summarize_tool_result(name, result);
                        let _ = event_tx
                            .send(Event::ToolCompleted { id, header, summary })
                            .await;
                    }
```

**Step 6 — status bar aggregate (D2).** In `ui/status.rs`, replace arm 6 (`if let Some((total,
done, label)) = app.active_tool()`, lines 130-140) — it always rendered `0/0` — and adjust arm 5
to collapse the duplicate brief:

```rust
        // 5. Live progress beats idle hints. When tool rows are visible
        //    in the viewport the chat already shows the live tool header;
        //    repeating the same brief here duplicates it. With several
        //    parallel calls, an aggregate is the honest line.
        if app.is_generating() {
            let running = app.running_tool_count();
            let brief_visible = app.show_tools() && app.is_scrolled_to_bottom();
            let phase = if running > 1 {
                format!("⚙ {running} tools running…")
            } else if matches!(app.phase(), crate::app::GenPhase::ExecutingTool(_)) && brief_visible
            {
                "working…".to_string()
            } else {
                app.phase_label().unwrap_or_else(|| "thinking…".to_string())
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} ", app.spinner_frame()),
                    Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
                ),
                Span::styled(phase, Style::default().fg(theme.warning)),
            ];
            if let Some(rate) = app.tokens_per_sec() {
                spans.push(Span::styled(format!(" · {:.0} tok/s", rate), dim));
            }
            spans.push(Span::styled(" · Esc cancels", dim));
            Widget::render(Line::from(spans), area, buf);
            return;
        }
```

Then **delete** `pub fn active_tool` from `app/ui_state.rs` (lines 717-723) — its only caller
was the removed arm. Run `rg -n 'active_tool' crates/` and remove any leftover references.

**Step 7 — migrate every construction site of the four events and of the four app methods.** Run:

```bash
rg -n 'Event::Tool(Started|Progress|Completed)' crates/kod-tui --no-heading
rg -n '(start_tool_execution|update_tool_status|complete_tool_execution|fail_tool_execution)\(' crates/kod-tui --no-heading
```

Every hit migrates mechanically: events gain the `id:` field (first), the app methods gain the
`id` parameter (first argument, `""` for fixtures that do not model provider ids). Known event
sites at `eb39247` (all must be migrated to the struct form; ids are `"".to_string()` in
fixtures that do not model provider ids):

- `main_loop.rs` inline tests: lines 6670, 6677, 6915, 6936, 6939, 6964, 6968, 6984, 7028, 7034,
  7065, 7075 — e.g. `Event::ToolStarted("execute_command".to_string())` becomes
  `Event::ToolStarted { id: String::new(), name: "execute_command".to_string() }`, and
  `Event::ToolCompleted("x".to_string(), "y".to_string())` becomes
  `Event::ToolCompleted { id: String::new(), header: "x".to_string(), summary: "y".to_string() }`.
- `tests/main_loop.rs` lines 180-184 — same mechanical change.
- `tests/app.rs`, `tests/event.rs`, `tests/swarm_e2e.rs` — migrate any hit the grep reports.

**Step 8 — regression tests for the whole phase.** Add to the inline test module in
`main_loop.rs` (next to the existing tool-row tests):

```rust
    #[tokio::test]
    async fn parallel_tool_calls_fill_their_own_rows() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::ToolStarted { id: "call_a".into(), name: "read_file".into() }).await;
        tui.handle_event(Event::ToolStarted { id: "call_b".into(), name: "read_file".into() }).await;
        tui.handle_event(Event::ToolProgress { id: "call_a".into(), display: "read_file path=a.rs".into() }).await;
        tui.handle_event(Event::ToolProgress { id: "call_b".into(), display: "read_file path=b.rs".into() }).await;
        tui.handle_event(Event::ToolCompletedWithDuration {
            id: "call_b".into(),
            header: "read_file path=b.rs".into(),
            summary: "b body".into(),
            duration_ms: 5,
        }).await;
        tui.handle_event(Event::ToolCompletedWithDuration {
            id: "call_a".into(),
            header: "read_file path=a.rs".into(),
            summary: "a body".into(),
            duration_ms: 7,
        }).await;

        let tools: Vec<&str> = tui
            .app()
            .messages()
            .iter()
            .filter(|m| m.role == kod_types::MessageRole::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(tools.len(), 2, "two calls, two rows: {tools:?}");
        let a = tools.iter().find(|t| t.contains("path=a.rs")).unwrap();
        let b = tools.iter().find(|t| t.contains("path=b.rs")).unwrap();
        assert!(a.contains("a body"), "row a carries result a: {a}");
        assert!(b.contains("b body"), "row b carries result b: {b}");
        assert!(a.contains("· 7ms"), "durations land on their own row: {a}");
        assert!(b.contains("· 5ms"), "durations land on their own row: {b}");
    }

    #[tokio::test]
    async fn task_end_fallback_after_live_done_marker_is_a_noop() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::ToolStarted { id: "call_a".into(), name: "read_file".into() }).await;
        tui.handle_event(Event::ToolCompletedWithDuration {
            id: "call_a".into(),
            header: "read_file path=a.rs".into(),
            summary: "live".into(),
            duration_ms: 9,
        }).await;
        // Task-end fallback arrives with a duration-less summary; it must
        // not touch the timed live row nor append a duplicate.
        tui.handle_event(Event::ToolCompleted {
            id: "call_a".into(),
            header: "read_file path=a.rs".into(),
            summary: "fallback".into(),
        }).await;

        let tools: Vec<&str> = tui
            .app()
            .messages()
            .iter()
            .filter(|m| m.role == kod_types::MessageRole::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(tools.len(), 1, "no duplicate row: {tools:?}");
        assert!(tools[0].contains("live"), "the timed live row is kept: {tools:?}");
        assert!(!tools[0].contains("fallback"));
    }

    #[tokio::test]
    async fn legacy_no_id_calls_still_complete_through_the_heuristic_path() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::ToolStarted { id: String::new(), name: "read_file".into() }).await;
        tui.handle_event(Event::ToolCompleted {
            id: String::new(),
            header: "read_file".into(),
            summary: "ok".into(),
        }).await;
        let tools: Vec<&str> = tui
            .app()
            .messages()
            .iter()
            .filter(|m| m.role == kod_types::MessageRole::Tool)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(tools.len(), 1);
        assert!(tools[0].contains("ok"));
    }
```

**Verify:**

```bash
cargo test -p kod-tui parallel_tool_calls_fill_their_own_rows
cargo test -p kod-tui task_end_fallback_after_live_done_marker_is_a_noop
cargo test -p kod-tui legacy_no_id_calls
cargo test -p kod-tui
cargo test -p kod-core --lib engine
cargo test -p kod-core --test tool_loop
cargo build --workspace
git add -A && git commit -m "tui(ux): U2/U3/D2 — call-id threading end to end (marker protocol v2)"
```

---

### Phase C — the visual diet (clutter + remaining mangling)

Note: defect **C3** (status line duplicating the tool brief, the `0/0` arm, the N>1 aggregate)
is delivered by Task B2 Step 6 — there is no separate C3 task here. Everything below is
independent; execute in order.

---

#### Task C1 (C2) — header: fewer segments, ordered by importance

**File:** `crates/kod-tui/src/ui/header.rs`

Current segment order (lines 29-139): `kod`, model, offline, ctx(+`ctx nearly full`), accounting
(`↑1.2k ↓340 · 5m30s`), cost, **`[theme]`**, sandbox (**always**, including `sandbox:off`),
`net:on`, goal. Problems: the theme badge is noise (discoverable via `/theme`); `sandbox:off`
states an absence the user did not ask about; the accounting triple + ctx percentage is
dashboard noise on a 1-row strip; and the goal — the one thing that identifies the *work* —
renders last, so on anything narrower than ~110 columns it is the first thing clipped.

**Replace** the whole `render` body (lines 22-143) with:

```rust
    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);

        let mut spans = vec![
            Span::styled(" kod ", title_style),
            Span::styled(format!("{} ", app.model_label()), dim),
        ];

        if app.is_offline() {
            spans.push(Span::styled(
                format!(" offline ×{} ", app.consecutive_failures()),
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Context meter: one figure, colored only when it matters. The
        // old label carried a trailing percentage that duplicated the
        // k/k fraction, and a separate "ctx nearly full" badge said in
        // words what the red color already says.
        let usage = app.context_usage();
        let ctx_style = if usage > 0.85 {
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD)
        } else if usage > 0.70 {
            Style::default().fg(theme.warning)
        } else {
            dim
        };
        spans.push(Span::styled(
            format!(" ctx ≈{}/{} ", app.context_tokens_k(), app.context_limit_k()),
            ctx_style,
        ));

        // USD cost, when the endpoint carries a `[pricing]` block. Not
        // shown at all when pricing is not configured — a fake `$0.0000`
        // teaches the user that the figure is meaningless.
        if app.cost_known() {
            spans.push(Span::styled(
                format!(" {} ", format_cost(app.session_cost_usd())),
                dim,
            ));
        }

        // The active goal identifies the work; it renders before the
        // state badges so narrow terminals clip the badges first.
        if let Some(goal) = app.goal() {
            const MAX_GOAL_DISPLAY_CHARS: usize = 40;
            let shown = if goal.chars().count() > MAX_GOAL_DISPLAY_CHARS {
                let truncated: String = goal.chars().take(MAX_GOAL_DISPLAY_CHARS).collect();
                format!("{truncated}…")
            } else {
                goal.to_string()
            };
            spans.push(Span::styled(
                format!(" ◉ {shown}"),
                Style::default().fg(Color::Magenta),
            ));
        }

        // Network indicator: visible whenever the effective network
        // access is enabled. The default is off, so this badge marks a
        // wider blast radius — it stays.
        if app.network_access_enabled() {
            spans.push(Span::styled(
                " net:on ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Sandbox badge. `off` is rendered as nothing: an absence of a
        // sandbox is the quiet default, and stamping it on every frame
        // is noise. `require-missing` (asked for, unavailable) and an
        // active backend (bwrap/landlock/…) stay visible.
        let sandbox = app.sandbox_label();
        if !sandbox.is_empty() && sandbox != "off" {
            let (style, label) = if sandbox == "require-missing" {
                (
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                    " sandbox:require-missing ".to_string(),
                )
            } else {
                (
                    Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                    format!(" sandbox:{sandbox} "),
                )
            };
            spans.push(Span::styled(label, style));
        }

        Widget::render(Line::from(spans), area, buf);
    }
```

**Supporting change — `crates/kod-tui/src/app/context.rs`:** add the two short formatters next
to `format_k` (lines 104-110) and keep `context_label` for `/context` and `/whoami` (it stays
used there):

```rust
    /// Short context figures for the header: `12.4k` / `128k`.
    pub fn context_tokens_k(&self) -> String {
        Self::format_k(self.context_tokens)
    }

    pub fn context_limit_k(&self) -> String {
        Self::format_k(self.context_limit)
    }
```

**Consequence check:** `rg -n 'accounting_label|theme_name' crates/kod-tui/src/ui` — the header
was their only ui/ caller. `accounting_label` and `theme_name` remain public for slash commands
(`/whoami`, `/theme`); leave them, they are now dead to `ui/` only. Add a header test:

```rust
    #[test]
    fn header_omits_the_theme_badge_and_the_off_sandbox() {
        let mut app = KodApp::new();
        app.set_model_name("qwen2.5-coder:7b");
        app.set_sandbox_label("off".to_string());
        let area = ratatui::layout::Rect::new(0, 0, 120, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        HeaderWidget::new().render(&app, area, &mut buf);
        let text: String = buf
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        assert!(text.contains("kod"), "{text}");
        assert!(text.contains("qwen2.5-coder:7b"), "{text}");
        assert!(text.contains("ctx ≈"), "{text}");
        assert!(!text.contains("[dark]"), "theme badge is gone: {text}");
        assert!(!text.contains("sandbox:off"), "off badge is gone: {text}");
    }
```

**Verify:** `cargo test -p kod-tui header`

---

#### Task C2 (C4) — tool rows preview 4 lines, not 12; shorter hint

**File:** `crates/kod-tui/src/ui/chat.rs`

The engine already caps command output summaries at 12 lines (`kod-core` `TOOL_RESULT_LINES`);
the TUI then shows all 12 of them inline for every finished tool. A chat where every `read_file`
or `execute_command` row costs 13 rows is the "tool calls show too much output" complaint.
Errors bypass the cap (auto-expanded).

**Change one constant** (line 18):

```rust
/// Tool body rows shown before collapsing to `… +N more lines (o expands)`.
pub const TOOL_DISPLAY_LINES: usize = 4;
```

**Change the hint text** (lines 267-280). The current string repeats three hints per collapsed
row (`o expands, t hides tools`); `t` is documented once in the footer that already exists
(`⋯ N tool output(s) hidden — t to show`). Replace the inner `format!` with:

```rust
                        format!(
                            "… +{} more lines (o expands)",
                            rows.len() - shown
                        ),
```

**Tests:** the existing `render_tool_body_over_the_cap_collapses_with_a_count` test asserts
`contains("more lines")` and `contains("o expands")` — both substrings survive, no change
needed. Add one boundary test:

```rust
    #[test]
    fn render_tool_preview_shows_four_lines_by_default() {
        let mut app = KodApp::new();
        let body = (0..10).map(|i| format!("row {i}")).collect::<Vec<_>>().join("\n");
        push_message(&mut app, MessageRole::Tool, &format!("[run] a tool\n{body}"));
        let text = render(&app, 100, 40);
        assert!(text.contains("row 3"), "4th line visible: {text}");
        assert!(!text.contains("row 4"), "5th line collapsed: {text}");
        assert!(text.contains("… +6 more lines (o expands)"), "{text}");
    }
```

**Verify:** `cargo test -p kod-tui render_tool`

---

#### Task C3 (B6) — word-boundary wrapping for plain text rows

**File:** `crates/kod-tui/src/ui/chat.rs`, `wrap_text` (lines 30-51).

The hand-rolled wrapper breaks at exactly `width` chars regardless of word boundaries, so user
messages, `sys` rows, and tool bodies hard-break mid-word ("hello wo / rld") while assistant
markdown wraps at words (markdown.rs `wrap_spans`). Because the pre-wrapped rows are all ≤
width, Ratatui's `Paragraph` (used for the final paint and for `line_count` measurement) never
re-wraps them — so fixing the wrapper cannot drift the scroll math: rows that fit are passed
through verbatim by `WordWrapper`.

**Replace** `wrap_text` with the word-aware version (same signature, same call sites):

```rust
    /// Split `s` into display rows of at most `width` cells. Wraps at
    /// word boundaries like the markdown path (`wrap_spans`); a word
    /// wider than the row (URLs, CJK runs without spaces) hard-breaks
    /// mid-word exactly like before. Tab characters wrap as spaces.
    /// Pre-wrapped rows are all ≤ width, so Ratatui's `WordWrapper`
    /// passes them through unchanged — measurement and paint agree.
    fn wrap_text(s: &str, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for raw in s.split('\n') {
            let mut cur = String::new();
            let mut cur_w = 0usize;
            let mut word = String::new();
            let mut word_w = 0usize;
            let flush_word = |cur: &mut String, cur_w: &mut usize, word: &mut String, word_w: &mut usize| {
                let ww = *word_w;
                if ww == 0 {
                    return;
                }
                // A word wider than the whole row hard-breaks in place.
                if ww > width {
                    for ch in word.chars() {
                        let w = Span::raw(ch.to_string()).width().max(1);
                        if *cur_w + w > width && !cur.is_empty() {
                            rows.push(std::mem::take(cur));
                            *cur_w = 0;
                        }
                        cur.push(ch);
                        *cur_w += w;
                    }
                } else {
                    if *cur_w > 0 && *cur_w + 1 + ww > width {
                        rows.push(std::mem::take(cur));
                        *cur_w = 0;
                    }
                    if *cur_w > 0 {
                        cur.push(' ');
                        *cur_w += 1;
                    }
                    cur.push_str(word);
                    *cur_w += ww;
                }
                word.clear();
                *word_w = 0;
            };
            for ch in raw.chars() {
                if ch == ' ' || ch == '\t' {
                    flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w);
                } else {
                    word.push(ch);
                    word_w += Span::raw(ch.to_string()).width().max(1);
                }
            }
            flush_word(&mut cur, &mut cur_w, &mut word, &mut word_w);
            rows.push(cur);
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        rows
    }
```

**Replace** the three mid-word `wrap_text` tests in the coverage module with:

```rust
    #[test]
    fn wrap_text_wraps_at_word_boundaries() {
        let rows = ChatWidget::wrap_text("hello brave world", 11);
        assert_eq!(rows, vec!["hello brave", "world"]);
    }

    #[test]
    fn wrap_text_hard_breaks_a_word_wider_than_the_row() {
        let rows = ChatWidget::wrap_text("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_text_keeps_multibyte_width_exact() {
        // CJK characters occupy two display cells each (per
        // `unicode-width`). No spaces → per-char hard break.
        let rows = ChatWidget::wrap_text("日本語です", 4);
        assert_eq!(rows, vec!["日本", "語で", "す"]);
    }

    #[test]
    fn wrap_text_handles_empty_input() {
        assert_eq!(ChatWidget::wrap_text("", 10), vec![""]);
    }
```

**Verify:** `cargo test -p kod-tui wrap_text && cargo test -p kod-tui`

---

#### Task C4 (B7) — approval popup never clips its key legend

**File:** `crates/kod-tui/src/ui/approval.rs`

`body_h = (lines.len() as u16 + 2).min(area.height)` counts logical lines, but the popup renders
with `Wrap { trim: false }` — a diff line longer than the popup width wraps into extra visual
rows, pushes the key legend below the popup bottom, and the user approves blind. Fix: compute
the popup width **first**, and truncate (not wrap) every content line to that width — the diff
is a preview; the full text stays in the tool row afterwards.

**Step 1** — add a width-aware truncate helper next to `truncate` (lines 199-208):

```rust
/// Truncate to `max` display cells (not chars), appending `…` when cut.
/// Wide CJK cells count as 2, so the result never overflows the popup.
fn truncate_width(s: &str, max: usize) -> String {
    if Span::raw(s).width() <= max {
        return s.to_string();
    }
    let mut acc = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = Span::raw(ch.to_string()).width().max(1);
        if w + cw > max.saturating_sub(1) {
            break;
        }
        acc.push(ch);
        w += cw;
    }
    acc.push('…');
    acc
}
```

**Step 2** — restructure `render`: compute the popup geometry before building lines, and route
every variable-length line through `truncate_width`. Concretely:

- Move `let body_w = 84u16.min(area.width).max(20);` and `let inner_w = body_w.saturating_sub(4) as usize;`
  (4 = 2 border cells + 2 leading-space padding) to just below the `let theme = …;` block.
- In the batch list, render `truncate(&item.summary, 40)` as before (it is already capped), and
  cap the tool name: `truncate_width(&item.tool_name, inner_w.saturating_sub(12))`.
- For the current item: `tool_name` → `truncate_width(&current.tool_name, inner_w.saturating_sub(9))`,
  `summary` → `truncate_width(&current.summary, inner_w.saturating_sub(9))` (9 = the `tool:    `/`summary: ` label width).
- In the diff loop (lines 102-115), replace the pushed span text with
  `truncate_width(l, inner_w)`.
- Keep `let body_h = (lines.len() as u16 + 2).min(area.height);` where it is — it is now exact,
  because no line can wrap anymore.
- Remove `Wrap { trim: false }` from the popup's `Paragraph` (it is now a no-op and documents
  the invariant): render with `Paragraph::new(lines).block(block)`.

**Test** (append to `tests/ui.rs`):

```rust
#[test]
fn approval_popup_legend_survives_a_wide_diff() {
    use kod_tui::app::{KodApp, PendingApproval, PendingApprovalBatch};
    use kod_tui::ui::ApprovalWidget;

    let mut app = KodApp::new();
    let long_line = format!("+{}", "x".repeat(300));
    app.set_pending_batch(PendingApprovalBatch {
        batch_id: 1,
        items: vec![PendingApproval {
            id: 1,
            tool_name: "write_file".to_string(),
            summary: "rewrite everything".to_string(),
            diff: Some(long_line),
            arguments: serde_json::json!({}),
        }],
        current: 0,
    });
    let area = ratatui::layout::Rect::new(0, 0, 100, 30);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    ApprovalWidget::new().render(&app, area, &mut buffer);
    let text: String = buffer
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    assert!(
        text.contains("approve"),
        "y-approve legend must be visible, not clipped: {text}"
    );
    assert!(text.contains("deny"), "deny legend must be visible: {text}");
}
```

**Verify:** `cargo test -p kod-tui approval_popup_legend`

---

### Phase D — one-shot notices and dedupe guards

All tasks independent; execute in order.

---

#### Task D1 (U4) — identical consecutive system rows collapse

**File:** `crates/kod-tui/src/app/messages.rs`, `push_system_message` (lines 142-151).

**Replace** with:

```rust
    pub fn push_system_message(&mut self, content: &str) {
        // Collapse exact consecutive duplicates: repeated engine notices
        // must not stack identical `sys` rows. Non-consecutive repeats
        // are legitimate (the same note can matter twice with context in
        // between) and are kept.
        if let Some(last) = self.messages.last()
            && last.role == MessageRole::System
            && last.content == content
        {
            return;
        }
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }
```

**Test:**

```rust
    #[test]
    fn consecutive_identical_system_rows_collapse() {
        let mut app = KodApp::new();
        app.push_system_message("restored 12 messages");
        app.push_system_message("restored 12 messages");
        assert_eq!(
            app.messages()
                .iter()
                .filter(|m| m.content == "restored 12 messages")
                .count(),
            1
        );
    }
```

**Verify:** `cargo test -p kod-tui consecutive_identical_system_rows_collapse`

---

#### Task D2 (C5) — the turn-duration notice leaves the transcript

**File:** `crates/kod-tui/src/main_loop.rs`, `Event::ResponseComplete` arm (lines ~805-815).

The bell (`\x07`) and the OSC 9 desktop notification already fire inside
`KodApp::notify_turn_complete`; the extra `(turn took {n}s — press any key to focus)` chat row
duplicates them and pollutes the transcript after every long turn. Session statistics keep
working — `/stats` reads the run collector, not the transcript.

**Replace:**

```rust
                // Notify only for turns longer than 30 seconds — a
                // quick exchange does not deserve a bell. The bell and
                // the OSC 9 desktop notification fire inside
                // notify_turn_complete; nothing is pushed into the
                // transcript (the old "(turn took Ns …)" row was noise
                // the bell already covers).
                self.app
                    .notify_turn_complete(std::time::Duration::from_secs(30));
```

(deleting the `if let Some(elapsed) = … { self.app.push_system_message(…) }` block). Check the
function's return value is otherwise unused: `rg -n 'notify_turn_complete' crates/kod-tui` — the
only other reference is the definition in `app/ui_state.rs`, which stays as-is.

**Test:** any test asserting `turn took` on system rows must be removed; add:

```rust
    #[tokio::test]
    async fn turn_completion_pushes_no_duration_row() {
        let mut tui = test_tui().await;
        tui.handle_event(Event::ResponseComplete("the answer".into())).await;
        assert!(tui
            .app()
            .messages()
            .iter()
            .all(|m| !m.content.contains("turn took")));
    }
```

**Verify:** `cargo test -p kod-tui turn_completion_pushes_no_duration_row`

---

#### Task D3 (C6) — the taint banner shows once per round, not once per turn

**File:** `crates/kod-tui/src/main_loop.rs`, `Event::ResponseComplete` arm (lines ~795-804), and
`crates/kod-tui/src/app/mod.rs`.

While a round stays tainted, every completed turn re-pushes the identical `⛨ round tainted …`
row. Track what was last shown.

**Step 1** — add the field to `KodApp` (near `last_error`) and to `KodApp::new()`:

```rust
    /// The taint banner last pushed, so a round that stays tainted does
    /// not re-announce itself on every completed turn. `None` when the
    /// round is clean or the banner has not been shown yet.
    last_taint_note: Option<String>,
```

```rust
            last_taint_note: None,
```

**Step 2** — replace the banner block in the `ResponseComplete` arm:

```rust
                // Tier 1.1 — surface a tainted round in one line, once
                // per round. Re-announcing on every turn of a long
                // round turned the banner into wallpaper.
                if let Some(engine) = self.engine.clone() {
                    let t = engine.taint_level();
                    if t.is_tainting() {
                        let note = format!(
                            "⛨ round tainted by {} — high-impact tools will ask. /trust show",
                            t.as_str(),
                        );
                        if self.app.last_taint_note.as_deref() != Some(note.as_str()) {
                            self.app.last_taint_note = Some(note.clone());
                            self.app.push_system_message(&note);
                        }
                    } else {
                        // Round is clean again (/trust clear, new round):
                        // re-arm the banner for the next taint.
                        self.app.last_taint_note = None;
                    }
                }
```

**Test:**

```rust
    #[tokio::test]
    async fn taint_banner_shows_once_until_the_round_is_clean() {
        let mut tui = test_tui().await;
        // Fake a persistent taint by pre-seeding the note the guard
        // compares against (the engine mock reports tainting).
        tui.app_mut().last_taint_note =
            Some("⛨ round tainted by untrusted-read — high-impact tools will ask. /trust show".into());
        tui.handle_event(Event::ResponseComplete("answer".into())).await;
        assert!(tui
            .app()
            .messages()
            .iter()
            .all(|m| !m.content.starts_with("⛨ round tainted")),
            "no repeat while the note is unchanged");
    }
```

(`app_mut()` stands for the test module's existing accessor pattern; if `TuiLoop` only exposes
`app()`, add `pub fn app_mut(&mut self) -> &mut KodApp { &mut self.app }` next to it — it is a
test-facing accessor, allowed.)

**Verify:** `cargo test -p kod-tui taint_banner_shows_once`

---

#### Task D4 (U5) — stop landing "[skills] used: …" rows in the transcript

**File:** `crates/kod-tui/src/main_loop.rs`, end of the `Ok(response)` branch of the streaming
task (lines ~1467-1474).

The skills a turn used are already visible: every skill-backed call renders its own tool row,
and `/skills` lists the inventory. A separate `[skills] used: …` agent row is bookkeeping in the
chat. Remove the emission; keep the `Event::AgentMessage` arm (the swarm path still sends agent
messages through other events — the arm stays harmless):

```rust
                    // Skills used this turn are visible through their own
                    // tool rows and /skills; no transcript row for the
                    // inventory. (The old "[skills] used: …" agent row
                    // was noise.)
```

i.e. **delete** the `if !response.skills_used.is_empty() { … }` block that sends
`Event::AgentMessage("skills", …)`.

**Verify:** `rg -n '"skills"' crates/kod-tui/src/main_loop.rs` returns no send site, then
`cargo test -p kod-tui && cargo test -p kod-core --lib engine`.

---

## Part 3 — Verification matrix

Run the full matrix after Phase D (and after any later cherry-pick):

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Automated behavior checks (must all pass)

| Requirement | Proven by |
|---|---|
| Tool error shown once | `tool_error_is_displayed_once_not_duplicated_as_a_system_line` (A1) |
| Parallel calls land in their own rows | `parallel_tool_calls_fill_their_own_rows` (B2) |
| Task-end fallback is idempotent | `task_end_fallback_after_live_done_marker_is_a_noop` (B2) |
| Legacy no-id calls still complete | `legacy_no_id_calls_still_complete_through_the_heuristic_path` (B2) |
| Marker v2 wire round-trips | `test_marker_v2_roundtrip_with_call_ids`, `test_tool_done_marker_roundtrip` (B1) |
| Error advice has no space runs | `friendly_error_advice_never_contains_double_spaces` (A2) |
| Multi-line provider errors collapse | `friendly_error_collapses_multiline_provider_dumps` (A4) |
| Failed turns keep partial answers | `fail_generation_keeps_the_partial_streamed_answer` (A3) |
| Raw errors never copied into tool rows | `fail_generation_never_copies_the_raw_error_into_tool_rows` (A3) |
| Engine notices reach the transcript | `system_events_reach_the_transcript_instead_of_vanishing` (A5) |
| Rule separates turns only | `render_draws_the_rule_only_before_user_turns` (A6) |
| Header is diet-clean | `header_omits_the_theme_badge_and_the_off_sandbox` (C1) |
| Tool preview caps at 4 lines | `render_tool_preview_shows_four_lines_by_default` (C2) |
| Plain text wraps at words | `wrap_text_wraps_at_word_boundaries` (C3) |
| Approval legend never clips | `approval_popup_legend_survives_a_wide_diff` (C4) |
| Duplicate sys rows collapse | `consecutive_identical_system_rows_collapse` (D1) |
| No turn-duration rows | `turn_completion_pushes_no_duration_row` (D2) |
| Taint banner once per round | `taint_banner_shows_once_until_the_round_is_clean` (D3) |

### Manual QA script (run `cargo run -p kod-tui` against any local OpenAI-compatible server)

1. **Turn shape** — send a prompt that triggers one tool call and a reply. Expect: `you` row, no
   rule, `⚙ … running…` row that fills in place with ≤ 4 preview lines + `… +N more lines (o
   expands)`, then the `╭─ ai ─╮` bubble. Exactly one `─` rule above the *next* `you` row.
2. **Expand/collapse** — press `o`: full capped body. Press `o` again: back to 4 lines. Press
   `t`: tool rows vanish except errors; footer shows `⋯ N tool output(s) hidden — t to show`.
3. **Parallel calls** — prompt something that makes two same-name calls in one round (two file
   reads). Both rows fill with their own results and durations; nothing duplicated, nothing
   swapped.
4. **Error path A (tool)** — have a tool fail (e.g. read a missing path). Expect exactly one
   `✗` row, auto-expanded, full error; **no** `Tool … failed:` sys row.
5. **Error path B (turn)** — stop the server mid-stream after some text streamed. Expect the
   partial answer preserved as `(error — partial answer)`, one friendly sys row (single line +
   advice), running rows showing `failed — see the error below`, no raw JSON anywhere.
6. **Header** — no `[theme]`, no `sandbox:off` (unless require-missing), ctx as `ctx ≈12.4k/128k`,
   goal visible before badges on an 80-column terminal.
7. **Status during tools** — with tools visible and viewport at bottom: `⠋ working… · 12 tok/s ·
   Esc cancels` (no duplicated tool brief). With tools hidden (`t`): the brief returns. With two
   parallel calls: `⚙ 2 tools running…`.
8. **Approval dialog** — trigger a wide diff (a 300-char line). Key legend (`y approve … Esc deny
   all`) fully visible; diff lines truncated with `…`, not wrapped.
9. **Restarts** — quit and rerun: the restored session renders in the same visual shape; no
   duplicate tool rows from the task-end fallback of the previous run (fallback events are not
   replayed, but a restored transcript must be byte-identical to what was on screen).

---

## Appendix A — event-construction migration sites (Task B2 Step 7)

At `eb39247`, the exhaustive list of `Event::Tool*` construction sites to migrate to the struct
variants (mechanical: add `id:` / rename fields):

```
crates/kod-tui/src/main_loop.rs   :875  arm        Event::ToolStarted(tool_name)        → { id, name }
crates/kod-tui/src/main_loop.rs   :882  arm        Event::ToolProgress(display)         → { id, display }
crates/kod-tui/src/main_loop.rs   :984  arm        Event::ToolCompleted(t, r)           → { id, header, summary }
crates/kod-tui/src/main_loop.rs   :999  arm        Event::ToolCompletedWithDuration(t,r,d) → { id, header, summary, duration_ms }
crates/kod-tui/src/main_loop.rs   :1357 send       Event::ToolStarted(tool)             → { id: cid, name }
crates/kod-tui/src/main_loop.rs   :1361 send       Event::ToolProgress(progress)        → { id: cid, display }
crates/kod-tui/src/main_loop.rs   :1367 send       Event::ToolCompletedWithDuration(…)  → { id: cid, … }
crates/kod-tui/src/main_loop.rs   :1465 send       Event::ToolCompleted(header, summary)→ { id, header, summary }
crates/kod-tui/src/main_loop.rs   :6670,6677,6915,6936,6939,6964,6968,6984,7028,7034,7065,7075 tests
crates/kod-tui/tests/main_loop.rs :180,184 tests
```

Anything else the grep in Step 7 reports (e.g. `tests/app.rs`, `tests/event.rs`,
`tests/swarm_e2e.rs`) migrates the same way.

## Appendix B — deliberately not done (and why)

- **`list_files` inline entry count (engine `TOOL_RESULT_LINES = 12`).** With the TUI preview at
  4 lines (Task C2) the display is bounded; expanding shows the engine's 12-line cap. Changing
  the engine cap would also change what `/export` and session JSON contain — out of scope.
- **`ttft` in the status line.** It appears once per turn and disappears; it is measurement, not
  noise. Keep.
- **Sashimi-grade: `Event::AgentMessage` arm retained though its only sender was the skills row.**
  The arm is two lines and keeps the event enum forward-compatible for swarm announcements.
- **Marker protocol versioning beyond v2.** The channel is in-process between crates that ship
  in the same workspace; there is no on-disk or cross-process consumer, so no version header is
  needed.
- **Theme-aware code-block background (`markdown.rs` hardcodes a dark bg for `Block::Code`).**
  A real (light-theme) fix belongs with a broader theme pass; it does not mangle content.
- **The status bar `tok/s` figure's char-based token estimate.** Approximate by design
  (`streamed_chars / 4`); replacing it with real streaming usage requires provider plumbing that
  has no UI in this plan.
