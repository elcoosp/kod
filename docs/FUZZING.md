# Fuzzing targets

The workspace's frame parsers are pure functions on byte slices — the
ideal shape for a fuzzer, and the shape where a malformed input from
outside the process (a network peer, a subprocess) becomes a panic
inside the agent.

Every one of these has had a real bug of the "malformed input panics"
class in the workspace's history. A `cargo fuzz` target per parser is
the standing regression.

## Targets

| Crate | Function | What a bug looks like |
|---|---|---|
| `kod-core` (acp) | `read_frame` | Length-prefix parser; a bad `Content-Length` must error, not allocate |
| `kod-core` (session_log) | `read_session` | A truncated last line must be tolerated, not fatal |
| `kod-core` (engine) | `parse_tool_done` / `parse_tool_start` / `parse_tool_args` | A chunk starting with `\0kod-` must roundtrip exactly |
| `kod-tools` (patch) | `parse_unified_diff` + `apply_unified_diff` | Malformed hunk headers must error, not index out of bounds |
| `kod-tools` (web) | `html_to_text` | Deeply nested tags, mixed encodings, no crash |
| `kod-lsp` (client) | `read_message` | Content-Length caps, header case, malformed JSON |
| `kod-mcp` (client) | `read_loop` line handler | Batch arrays, ids as strings, huge payloads |
| `kod-provider-anthropic` (wire) | `parse_sse_line` | Multi-byte UTF-8 split across chunks, malformed JSON, unknown events |
| `kod-memory` (embedding) | `parse_float_array` | NaN, infinity, wrong shapes, absurd lengths |
| `kod-config` (policy) | `Policy::from_toml` | Missing sections, wrong types, deeply nested |

## Running

```sh
cargo install cargo-fuzz
cargo fuzz list
cargo fuzz run parse_unified_diff
```

A crasher is a bug in the parser, not in the fuzzer. Fix it and add a
reproduction test to the parser's own test module before adding a
`##[corpus]` entry.
