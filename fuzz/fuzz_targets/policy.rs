#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // A hostile policy.toml (bad types, deeply-nested tables,
        // unknown keys) must fail cleanly, not panic.
        let _ = kod_config::policy::Policy::from_toml(s);
    }
});
