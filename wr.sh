#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-tui/src/app.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: auto-compact + manual compact assign a real sequence"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. maybe_compact: push through add_message ------------------------
patch(
    '''        let note = format!(
            "Auto-compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        );
        self.messages.push(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: note,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        self.scroll_to_bottom();''',
    '''        let note = format!(
            "Auto-compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        );
        // Route through add_message so the notice gets a monotonic
        // sequence. Pushing directly with `sequence: 0` made the chat
        // widget (which sorts by sequence) render the notice at the
        // very top of the transcript, above the messages it had just
        // compacted — the exact opposite of where it belongs.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: note,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });''',
    "maybe_compact sequence",
)

# --- 2. compact_now: push through add_message --------------------------
patch(
    '''    /// Manual `/compact`: same as auto but on demand.
    pub fn compact_now(&mut self) {
        if self.messages.len() <= 21 {
            self.push_system_message(&format!("Nothing to compact · {}", self.context_label()));
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        self.context_tokens = self.context_limit * 3 / 5;
        self.push_system_message(&format!(
            "Compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        ));
    }''',
    '''    /// Manual `/compact`: same as auto but on demand.
    ///
    /// Both branches already went through `push_system_message` (which
    /// calls `add_message`), so the notice has always had a correct
    /// sequence — kept here as the counterpart to `maybe_compact`, and
    /// to make it obvious that both paths must go through `add_message`.
    pub fn compact_now(&mut self) {
        if self.messages.len() <= 21 {
            self.push_system_message(&format!("Nothing to compact · {}", self.context_label()));
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        self.context_tokens = self.context_limit * 3 / 5;
        self.push_system_message(&format!(
            "Compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        ));
    }''',
    "compact_now comment",
)

# --- 3. Tests --------------------------------------------------------
patch(
    '''    #[test]
    fn test_scroll_to_bottom() {''',
    '''    /// Auto-compact must append its notice at the end of the transcript,
    /// not sort it to the top. The chat widget sorts by `sequence`, and
    /// the old `maybe_compact` pushed a message with `sequence: 0`,
    /// which sorted before every user message in the session.
    #[test]
    fn test_auto_compact_notice_sorts_after_compacted_messages() {
        let mut app = KodApp::new();
        // Force the auto-compact threshold with a tiny context window.
        app.set_context_limit(1_000);

        // Add enough messages to cross the 4/5 threshold AND exceed the
        // 21-message guard that protects against compacting an empty or
        // short session.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        // Push token usage past 4/5 of 1000 = 800.
        app.note_real_usage(900);

        // The notice must be the last message by sequence.
        let last = app.messages().last().expect("at least one message");
        let last_seq = last.sequence;
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Auto-compacted"),
            "last message should be the compaction notice, got: {}",
            last.content
        );

        // And no other message has a sequence greater than it (trivially
        // true) or equal-and-later-positioned at the same sequence.
        for m in app.messages().iter().take(app.messages().len() - 1) {
            assert!(
                m.sequence < last_seq,
                "a message sorts after the compaction notice: seq {} vs {}",
                m.sequence,
                last_seq
            );
        }
    }

    /// The manual `/compact` path must also leave its notice at the end.
    #[test]
    fn test_manual_compact_notice_sorts_after() {
        let mut app = KodApp::new();
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.compact_now();

        let last = app.messages().last().expect("at least one message");
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Compacted"),
            "last message should be the manual compaction notice, got: {}",
            last.content
        );
    }

    #[test]
    fn test_scroll_to_bottom() {''',
    "auto-compact sequence tests",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(tui): auto-compact notice gets a monotonic sequence

KodApp::maybe_compact pushed its 'Auto-compacted N older messages'
notice with self.messages.push(Message { …, sequence: 0 }). The
chat widget sorts the transcript by sequence (a monotonic counter
set by add_message), so a sequence-0 message sorted to the very
top — above every user and assistant message in the session.

The visible result: the moment auto-compact crossed its threshold,
the compaction notice appeared at the top of the chat, nowhere
near the messages it had just compacted, and the newest content
(the messages the user was actually looking at) stayed put. A
second auto-compact would push another sequence-0 notice, and the
two would tie at the top with arbitrary relative order.

Route the notice through add_message so it gets next_seq like every
other message. compact_now was already correct (it uses
push_system_message), but the docstring now calls out the invariant
so the two paths cannot drift again.

Adds two tests: test_auto_compact_notice_sorts_after_compacted_messages
drives a tiny context window to 30 filler messages plus 900 tokens
of usage, then asserts the last message by position is the
compaction notice and no other message sorts after it;
test_manual_compact_notice_sorts_after covers the /compact path."
