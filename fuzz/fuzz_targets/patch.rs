#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // The parser is a pure function; a panic or an out-of-bounds
        // slice is the bug class we are hunting.
        let _ = kod_tools::patch::parse_unified_diff(s);
        let _ = kod_tools::patch::apply_unified_diff("fn main() {}\n", s);
    }
});
