#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // SEC-B-06: exercise the ACP Content-Length + JSON framing state
    // machine on arbitrary bytes. `read_frame_for_fuzz` builds a
    // single-thread runtime internally and drives `read_frame` over
    // the slice; the loop always terminates because `&[u8]` yields
    // EOF once consumed.
    let _ = kod_core_serve::acp::read_frame_for_fuzz(data);
});
