#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Deeply-nested tags, mixed encodings, and unterminated
        // elements must not panic.
        let _ = kod_tools::web::html_to_text_for_fuzz(s);
    }
});
