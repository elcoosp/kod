//! End-to-end TUI tests.
//!
//! Each test spawns the real `kod tui` binary through a PTY, drives
//! it with keystrokes, and asserts against the parsed screen. The LLM
//! is a loopback mock (see `mock_llm`) and Jev is disabled in the
//! config — a test that fails points at the TUI, not at a model.

mod harness;
mod mock_llm;

use harness::{TestEnv, WAIT};

/// Smallest end-to-end proof: the process starts, reads the test
/// config, builds the engine, paints the first frame, and dispatches
/// the startup system message.
#[test]
fn startup_reaches_the_ready_state() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    let screen = harness::wait_for_ready(&session);

    // The header shows the model name from the config, not the
    // built-in default — proof the config override landed.
    assert!(
        screen.contains("mock-model"),
        "header should show the configured model; got:\n{screen}"
    );
    // And the startup system message proves the event loop is live.
    assert!(
        screen.contains("Connected"),
        "startup system message missing; got:\n{screen}"
    );
}
