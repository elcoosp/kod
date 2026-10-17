#![no_main]
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            let spec = kod_schema_dialect::DialectSpec::default();
            let _ = kod_schema_dialect::sanitize::sanitize(&v, &spec);
        }
    }
});
