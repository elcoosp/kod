#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let mut state = kod_provider_anthropic::wire::AnthropicStreamState::default();
        for line in s.lines() {
            let _ = kod_provider_anthropic::wire::parse_sse_line(&mut state, line);
        }
    }
});
