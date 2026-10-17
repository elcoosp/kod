#![no_main]
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            // Construct a DialectSpec via the API available in this
            // crate version. `spec_for_provider("anthropic")` returns
            // a valid spec (Anthropic's tool-schema dialect) without
            // requiring a `Default` impl that may not exist.
            let spec = kod_schema_dialect::sanitize::spec_for_provider("anthropic");
            let _ = kod_schema_dialect::sanitize::sanitize(&v, &spec);
        }
    }
});
