#!/usr/bin/env bash
set -uo pipefail

APP=crates/kod-tui/src/app.rs
LOOP=crates/kod-tui/src/main_loop.rs

for f in "$APP" "$LOOP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

echo "Patching $APP: suppress char estimate after real usage arrives"

python3 - "$APP" "$LOOP" << 'PYEOF'
import os
import sys

app, loop = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"ERROR: anchor not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(src.replace(old, new, expect if expect else n))
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# ======================================================================
# 1. New field on KodApp.
# ======================================================================
patch(
    app,
    '''    /// Rough session token accounting (1 token ≈ 4 chars over prompts +
    /// replies). Drives the context meter and auto-compact.
    context_tokens: usize,''',
    '''    /// Rough session token accounting (1 token ≈ 4 chars over prompts +
    /// replies). Drives the context meter and auto-compact.
    context_tokens: usize,
    /// Set when the provider has reported real token usage for the
    /// current turn. Once set, the char-based estimate stops
    /// contributing to `context_tokens` until the next
    /// `begin_generation`.
    ///
    /// The provider's `usage` totals are authoritative and already
    /// include every token the model saw (prompt + completion). The
    /// estimate that `note_usage` accumulates from
    /// `note_prompt` / `flush_streamed_text` / `finish_response` also
    /// runs across the same turn. Without this flag the two stacked:
    /// TokenUsage arrived, replaced the estimate with the real total,
    /// and then finish_response immediately added the assistant's
    /// chars/4 on top — over-reporting the context by roughly the
    /// reply length every turn.
    turn_has_real_usage: bool,''',
    "turn_has_real_usage field",
)

# ======================================================================
# 2. Initialise it in KodApp::new.
# ======================================================================
patch(
    app,
    '''            context_tokens: 0,
            context_limit: DEFAULT_CONTEXT_LIMIT,
            compacted_messages: 0,''',
    '''            context_tokens: 0,
            turn_has_real_usage: false,
            context_limit: DEFAULT_CONTEXT_LIMIT,
            compacted_messages: 0,''',
    "init turn_has_real_usage",
)

# ======================================================================
# 3. begin_generation resets the flag.
# ======================================================================
patch(
    app,
    '''    pub fn begin_generation(&mut self) {
        self.generating = true;
        self.spinner_started = Some(Instant::now());
        self.stream_flushed_bubble = false;
        self.last_error = None;
        self.set_phase(GenPhase::Connecting);
        self.start_response_stream();
    }''',
    '''    pub fn begin_generation(&mut self) {
        self.generating = true;
        self.spinner_started = Some(Instant::now());
        self.stream_flushed_bubble = false;
        self.last_error = None;
        // New turn: no real usage seen yet, so the char estimate
        // contributes until (or unless) the provider reports a total.
        self.turn_has_real_usage = false;
        self.set_phase(GenPhase::Connecting);
        self.start_response_stream();
    }''',
    "begin_generation resets flag",
)

# ======================================================================
# 4. note_real_usage sets the flag.
# ======================================================================
patch(
    app,
    '''    pub fn note_real_usage(&mut self, total_tokens: usize) {
        if total_tokens > 0 {
            self.context_tokens = total_tokens;
        }
        self.maybe_compact();
    }''',
    '''    pub fn note_real_usage(&mut self, total_tokens: usize) {
        if total_tokens > 0 {
            self.context_tokens = total_tokens;
            // Once real usage has arrived, the char estimate must stop
            // contributing for the rest of the turn. The provider's
            // total already covers prompt + completion, so any
            // subsequent `note_usage` from `finish_response` /
            // `flush_streamed_text` would double-count the reply.
            self.turn_has_real_usage = true;
        }
        self.maybe_compact();
    }''',
    "note_real_usage sets flag",
)

# ======================================================================
# 5. note_usage is gated.
# ======================================================================
patch(
    app,
    '''    fn note_usage(&mut self, chars: usize) {
        self.context_tokens = self
            .context_tokens
            .saturating_add(Self::estimate_tokens(chars));
    }''',
    '''    fn note_usage(&mut self, chars: usize) {
        // Suppressed once real usage for this turn has been recorded.
        // See `turn_has_real_usage` for the double-count this avoids.
        if self.turn_has_real_usage {
            return;
        }
        self.context_tokens = self
            .context_tokens
            .saturating_add(Self::estimate_tokens(chars));
    }''',
    "note_usage gated",
)

# ======================================================================
# 6. Reset the flag on cancel / fail so the next turn starts clean
#    even if begin_generation is not reached.
# ======================================================================
patch(
    app,
    '''    pub fn cancel_generation(&mut self) {
        let partial = Self::trim_blank_lines(&self.current_response);''',
    '''    pub fn cancel_generation(&mut self) {
        // The turn is over; a subsequent prompt must see a fresh
        // turn state even if begin_generation is not called before
        // next note_prompt. begin_generation resets this again.
        self.turn_has_real_usage = false;
        let partial = Self::trim_blank_lines(&self.current_response);''',
    "cancel_generation resets flag",
)

patch(
    app,
    '''    pub fn fail_generation(&mut self, error: &str) {
        self.is_streaming = false;''',
    '''    pub fn fail_generation(&mut self, error: &str) {
        // Same rationale as cancel_generation: settle the flag so the
        // next turn does not inherit "real usage seen" from this one.
        self.turn_has_real_usage = false;
        self.is_streaming = false;''',
    "fail_generation resets flag",
)

# ======================================================================
# 7. Test.
# ======================================================================
patch(
    app,
    '''    /// `search_status` must distinguish the states the old''',
    '''    /// Real token usage replaces the char estimate, and subsequent
    /// estimate calls in the same turn must not stack on top of it.
    /// Regression: TokenUsage arrives before ResponseComplete in the
    /// event queue; finish_response then called note_usage, adding
    /// the reply's chars/4 to a total that already included the
    /// completion — every turn over-reported by roughly the reply
    /// length.
    #[test]
    fn test_real_usage_suppresses_further_estimates() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        // New turn: estimate-only until real usage arrives.
        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000 estimate
        assert!(app.context_tokens() >= 1_000);
        let after_prompt = app.context_tokens();

        // Provider reports the real total (already includes the
        // completion). Replaces the estimate.
        app.note_real_usage(1_500);
        assert_eq!(app.context_tokens(), 1_500);

        // finish_response adds the reply's chars/4 via note_usage;
        // gated by the flag, it must be a no-op.
        let reply = "y".repeat(2_000); // would be +500 if applied
        app.finish_response(&reply);
        assert_eq!(
            app.context_tokens(),
            1_500,
            "post-usage estimate must not stack (was {} before reply, {} after)",
            after_prompt,
            app.context_tokens(),
        );
    }

    /// A turn where the provider reports no usage must fall back to
    /// the char estimate, unchanged from before.
    #[test]
    fn test_estimate_still_works_without_real_usage() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000
        let after_prompt = app.context_tokens();
        assert!(after_prompt >= 1_000);

        app.finish_response(&"y".repeat(4_000)); // ≈ +1_000
        assert!(
            app.context_tokens() > after_prompt,
            "estimate must accumulate when no real usage arrives"
        );
    }

    /// `search_status` must distinguish the states the old''',
    "turn_has_real_usage tests",
)

print("Done.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -20"
if ! cargo check --workspace --all-targets 2>&1 | tail -20; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20; then
    echo "Clippy failed"
    exit 1
fi

echo
echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(tui): real token usage suppresses further char estimates in the turn

The pump in dispatch_prompt sends Event::TokenUsage (real totals
from the provider) before Event::ResponseComplete. Event::TokenUsage
calls KodApp::note_real_usage, which replaces context_tokens with
the authoritative value. Then Event::ResponseComplete calls
KodApp::finish_response, which calls note_usage(text.len()) —
adding the assistant reply's chars/4 on top of a number that
already included the completion tokens.

For a one-round turn this over-reported by roughly the reply length
every turn. For a multi-round agentic turn, flush_streamed_text at
each tool boundary stacked pre-tool text estimates on top of a real
total that arrives only at the end, so the visible meter during the
turn could be much larger than the eventual real number, and
maybe_compact could fire mid-turn on an inflated reading.

Add a `turn_has_real_usage: bool` on KodApp. begin_generation
resets it; note_real_usage sets it when a nonzero total arrives;
note_usage is a no-op once it is set. cancel_generation and
fail_generation also reset it so a subsequent turn that never
reaches begin_generation (rare, but possible) starts clean.

The char estimate remains in place for providers that omit usage —
some local OpenAI-compatible servers do — and the second test pins
that path: with no TokenUsage event, note_prompt and
finish_response accumulate as before.

Two tests: real usage replaces the estimate and blocks the
subsequent finish_response estimate; no real usage lets the
estimate accumulate normally.
MSG
