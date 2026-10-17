#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // SEC-B-06: exercise the MCP line-parse path on arbitrary UTF-8
    // bytes. Non-UTF-8 is dropped (matches `read_loop`'s `String`
    // expectation for a well-formed line).
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = kod_mcp::client::parse_line_for_fuzz(s);
    }
});
