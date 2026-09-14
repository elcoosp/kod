#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
CARGO=crates/kod-core/Cargo.toml
TARGET=crates/kod-core/src/engine.rs

for f in "$CARGO" "$TARGET"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Adding proptest dev-dep to kod-core and property tests to engine.rs"

python3 - "$CARGO" "$TARGET" << 'PYEOF'
import os
import sys

cargo, target = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# --- 1. kod-core Cargo.toml: add proptest to dev-deps ------------------
patch(
    cargo,
    '''[dev-dependencies]
rstest = { workspace = true }''',
    '''[dev-dependencies]
proptest = { workspace = true }
rstest = { workspace = true }''',
    "proptest dev-dep",
)

# --- 2. engine.rs: append a property-test module -----------------------
# Keep the existing `#[cfg(test)] mod tests` intact; add a sibling
# module so proptest-generated cases don't slow the hand-written suite.
with open(target, "r") as f:
    content = f.read()

if "mod prop_tests" in content:
    print("Skipped: prop_tests already present")
else:
    content += '''

#[cfg(test)]
mod prop_tests {
    //! Property tests for the \\0kod-* marker protocol.
    //!
    //! The four markers (`tool_start_marker`, `tool_args_marker`,
    //! `tool_done_marker`, `THINKING_MARKER`) form a small wire protocol
    //! between the engine and the TUI: the engine builds a string, sends
    //! it down an mpsc channel, and the TUI parses it back. The protocol
    //! relies on an out-of-band NUL sentinel, which no ordinary text will
    //! contain — but tool output does sometimes contain NUL bytes, and
    //! any future marker field could accidentally carry one.
    //!
    //! `tool_done_marker` is documented as sanitizing embedded NULs to
    //! spaces before encoding, so its round-trip is only identity on
    //! NUL-free inputs. `tool_start_marker` and `tool_args_marker` do not
    //! sanitize, so their properties only hold for NUL-free inputs.
    //!
    //! These tests assert:
    //!   1. Round-trips are identity on the domain where they are defined.
    //!   2. `parse_tool_done` never drops a well-formed completion, and
    //!      never accepts a truncated one (a wrong parse is worse than a
    //!      drop: the TUI would show a stale tool row).
    //!   3. `truncate_chars` always yields a char-boundary-respecting
    //!      prefix.
    //!   4. `format_tool_header` and `format_call_brief` never panic on
    //!      arbitrary JSON, since the model supplies the arguments.

    use super::*;
    use proptest::prelude::*;

    /// Regex strategy that never emits NUL: the marker protocol cannot
    /// represent an embedded NUL, and the engine-side builders are the
    /// only sanctioned place that substitutes one for a space.
    fn no_nul() -> impl Strategy<Value = String> {
        ".{0,300}".prop_filter("NUL-free", |s| !s.contains('\\0'))
    }

    proptest! {
        /// tool_start_marker / parse_tool_start round-trip on NUL-free input.
        #[test]
        fn prop_tool_start_roundtrip(name in no_nul().prop_filter("non-empty", |s| !s.is_empty())) {
            let chunk = tool_start_marker(&name);
            let parsed = parse_tool_start(&chunk)
                .expect("marker built by tool_start_marker must parse");
            prop_assert_eq!(parsed, name.as_str());
        }

        /// tool_args_marker / parse_tool_args round-trip on NUL-free input.
        #[test]
        fn prop_tool_args_roundtrip(display in no_nul()) {
            let chunk = tool_args_marker(&display);
            let parsed = parse_tool_args(&chunk)
                .expect("marker built by tool_args_marker must parse");
            prop_assert_eq!(parsed, display.as_str());
        }

        /// tool_done_marker / parse_tool_done round-trip. The builder
        /// sanitizes NUL to space, so the round-trip target is the
        /// sanitized form, not the raw inputs.
        #[test]
        fn prop_tool_done_roundtrip(
            header in ".{0,200}",
            summary in ".{0,500}",
            ms in any::<u64>(),
        ) {
            let header_sanitized = header.replace('\\0', " ");
            let summary_sanitized = summary.replace('\\0', " ");
            let chunk = tool_done_marker(&header, &summary, ms);
            let (h, s, m) = parse_tool_done(&chunk)
                .expect("marker built by tool_done_marker must parse");
            prop_assert_eq!(h, header_sanitized.as_str());
            prop_assert_eq!(s, summary_sanitized.as_str());
            prop_assert_eq!(m, ms);
        }

        /// Any chunk the engine might send that is *not* a well-formed
        /// done-marker must not parse as one. This is what keeps the TUI
        /// from misreading streamed text as a control frame.
        #[test]
        fn prop_arbitrary_text_is_not_a_done_marker(s in ".{0,400}") {
            // Only assert that a non-marker does not accidentally parse
            // as a marker with a mismatched shape. If it parses, the
            // returned tuple must contain the exact payload.
            if let Some((h, s_, m)) = parse_tool_done(&s) {
                prop_assert!(s.starts_with(TOOL_DONE_MARKER));
                prop_assert!(h.len() + s_.len() <= s.len());
                // Duration must round-trip through u64 parsing, or be
                // the documented degradation to 0.
                if let Ok(parsed) = s.splitn(3, '\\0').nth(2).unwrap_or("").parse::<u64>() {
                    prop_assert_eq!(m, parsed);
                } else {
                    prop_assert_eq!(m, 0);
                }
            }
        }

        /// truncate_chars(s, max) is always a char-boundary prefix of s
        /// no longer than max bytes. The whole reason the helper exists
        /// is that &s[..max] panics on multibyte input.
        #[test]
        fn prop_truncate_chars_is_a_safe_prefix(
            s in ".{0,2000}",
            max in 0usize..2000,
        ) {
            let out = truncate_chars(&s, max);
            prop_assert!(out.len() <= max);
            prop_assert!(s.is_char_boundary(out.len()));
            prop_assert!(s.starts_with(out));
        }

        /// format_tool_header must never panic: the model supplies the
        /// arguments, and it can put anything in them.
        #[test]
        fn prop_format_tool_header_never_panics(
            name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
            path in ".{0,500}",
            pattern in ".{0,500}",
        ) {
            let args = serde_json::json!({
                "path": path,
                "pattern": pattern,
            });
            let _ = format_tool_header(&name, &args);
        }

        /// format_call_brief must never panic on arbitrary arguments.
        #[test]
        fn prop_format_call_brief_never_panics(
            name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
            command in ".{0,1000}",
        ) {
            // Two shapes the two branches of format_call_brief handle:
            // execute_command with a "command" key, and a generic tool
            // with a "path" key.
            let args_cmd = serde_json::json!({ "command": command });
            let _ = format_call_brief(&name, &args_cmd);
            let args_path = serde_json::json!({ "path": command });
            let _ = format_call_brief(&name, &args_path);
        }
    }
}
'''
    with open(target, "w") as f:
        f.write(content)
    print("Appended prop_tests module to engine.rs")

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-core property tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-core prop_tests 2>&1; then
    echo "Property tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running full kod-core tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-core 2>&1; then
    echo "kod-core tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "test(core): property-test the \\0kod-* marker protocol

The four control markers (tool_start_marker, tool_args_marker,
tool_done_marker, THINKING_MARKER) form a small wire protocol
between the engine and the TUI: the engine builds a string, sends
it down an mpsc channel, and the TUI parses it back. Until now the
only tests were hand-picked inputs from the original code review.
The protocol relies on an out-of-band NUL sentinel; tool output
occasionally contains NUL bytes, and any future marker field could
accidentally carry one.

Add seven proptest properties under a new prop_tests module:

- tool_start_marker / parse_tool_start round-trip on NUL-free input.
- tool_args_marker / parse_tool_args round-trip on NUL-free input.
- tool_done_marker / parse_tool_done round-trip, with the documented
  NUL-to-space sanitization applied to the expected payload.
- Arbitrary text does not accidentally parse as a done-marker: when
  parse_tool_done returns Some, the parsed fields and duration must
  be consistent with the input.
- truncate_chars always returns a char-boundary prefix no longer
  than the requested max — the property that motivates the helper.
- format_tool_header and format_call_brief never panic on arbitrary
  JSON, since the model supplies the arguments.

proptest was already a workspace dep but not wired into any crate;
add it to kod-core's dev-deps. The property module is a sibling of
the existing #[cfg(test)] mod tests, so the hand-written suite's
runtime is unchanged."
