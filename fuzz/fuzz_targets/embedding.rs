#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The vector parser accepts a JSON Value; fuzz it with arbitrary
    // bytes converted through a Value construction.
    if let Ok(s) = std::str::from_utf8(data)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(s)
    {
        let _ = kod_memory::embedding::parse_float_array_for_fuzz(&v);
    }
});
