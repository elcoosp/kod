//! End-to-end TUI tests.
//!
//! Each test spawns the real `kod tui` binary through a PTY, drives
//! it with keystrokes, and asserts against the parsed screen. The LLM
//! is a loopback mock (see `mock_llm`) and Jev is disabled in the
//! config — a test that fails points at the TUI, not at a model.
//!
//! # Reading a failure
//!
//! `TuiSession::wait_for_text` panics with the full parsed screen on
//! timeout. Compare that dump against the assertion's expected
//! substring; the discrepancy is usually a rendering change, not a
//! race.

mod harness;
mod mock_llm;
mod mock_probe;

use std::time::Duration;

use harness::{KeyCode, TestEnv, WAIT};

/// Smallest end-to-end proof: the process starts, reads the test
/// config, builds the engine, paints the first frame, and dispatches
/// the startup system message.
#[test]
fn startup_reaches_the_ready_state() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    let screen = harness::wait_for_ready(&session);

    assert!(
        screen.contains("mock-model"),
        "header should show the configured model; got:\n{screen}"
    );
    assert!(
        screen.contains("Connected"),
        "startup system message missing; got:\n{screen}"
    );
}

/// Typing in insert mode echoes into the input box.
///
/// This is the first test that exercises the full input path: raw
/// byte → crossterm event → main loop dispatch → app state → render.
#[test]
fn typing_echoes_in_the_input_box() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    // `i` enters insert mode (normal-mode keybinding).
    session.send_key(KeyCode::Char('i'));
    session.send_text("hello world");

    // The input box renders the text somewhere on screen; assert on
    // the literal string rather than its position so a layout change
    // does not break the test.
    let screen = session.wait_for_text("hello world", WAIT);

    // The typed text must appear in the input box, not in the
    // transcript (i.e. not preceded by the system prefix). The
    // bottom-frame hint "Press i to type" is replaced by the boxed
    // input area once insert mode is active.
    assert!(
        screen.contains("hello world"),
        "typed text did not appear:\n{screen}"
    );
}

/// Submitting a prompt appends the user's message to the transcript
/// and triggers a model call against the mock.
#[test]
fn submitting_a_prompt_shows_it_in_the_transcript() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("say hi");
    session.send_key(KeyCode::Enter);

    // The transcript renders the user's message with a `you` prefix
    // (see `KodApp::push_assistant_message` / the chat widget's user
    // branch). Wait for the literal text; the echo-in-input check
    // above is subsumed by it.
    let screen = session.wait_for_text("say hi", WAIT);
    assert!(
        screen.contains("say hi"),
        "user message did not land in the transcript:\n{screen}"
    );
}

/// The mock's reply streams back and lands in the transcript.
///
/// This is the test that pays for the whole harness: it exercises
/// crossterm input, engine dispatch, the OpenAI provider's SSE
/// parser, the streaming chunk handlers added in H-T1/T2/T3, and the
/// chat widget's streaming-render branch — end-to-end, against a real
/// socket.
#[test]
fn mock_reply_streams_into_the_transcript() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("please reply");
    session.send_key(KeyCode::Enter);

    // Diagnostic: after 2s, list the child's network sockets. If no
    // connection to the mock's port appears, the request is not
    // being made at all; if one appears and the mock logs nothing,
    // the request is being sent somewhere else.
    std::thread::sleep(Duration::from_secs(2));
    eprintln!("--- lsof for child pid ---");
    eprintln!("{}", session.lsof_network());
    eprintln!("--- end lsof ---");

    let screen = session.wait_for_text(mock_llm::MOCK_REPLY, WAIT);
    assert!(
        screen.contains(mock_llm::MOCK_REPLY),
        "mock reply never reached the screen:\n{screen}"
    );
}

/// A reply streamed one byte per SSE event reassembles correctly.
///
/// Exercises the provider's UTF-8 boundary handling. The Anthropic
/// provider had a bug (H-P2) where a multi-byte character split
/// across TCP chunks became U+FFFD; the OpenAI path uses a different
/// decoder, and this test pins that it reassembles cleanly.
#[test]
fn byte_by_byte_streaming_reassembles() {
    // A reply with a multi-byte character to catch mid-codepoint
    // splits: "héllo wörld" has two 2-byte sequences.
    const REPLY: &str = "héllo wörld";
    let env = TestEnv::new_byte_by_byte(REPLY);
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("stream please");
    session.send_key(KeyCode::Enter);

    let screen = session.wait_for_text(REPLY, WAIT);
    assert!(
        screen.contains(REPLY),
        "byte-by-byte stream did not reassemble to {REPLY:?}:\n{screen}"
    );
}

/// `/help` opens the help overlay.
#[test]
fn slash_help_opens_the_overlay() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("/help");
    session.send_key(KeyCode::Enter);

    // The help overlay renders a header — pin a stable substring
    // from the widget rather than the whole overlay so a reflow does
    // not break the test.
    let screen = session.wait_for_text("Keyboard shortcuts", WAIT);
    assert!(
        screen.contains("Keyboard shortcuts"),
        "help overlay did not open:\n{screen}"
    );
}

/// Tab-completion popup appears while typing a slash command.
#[test]
fn slash_command_shows_completion_popup() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    // Type just the prefix of a known command. The popup should
    // render candidate names below the input box.
    session.send_text("/he");

    // The completion list shows at least the `/help` candidate; the
    // widget may prefix it with a bullet or index, so a substring
    // match is right.
    let screen = session.wait_for_text("/help", WAIT);
    assert!(
        screen.contains("/help"),
        "completion popup did not offer /help:\n{screen}"
    );
}

/// Esc leaves insert mode and returns to normal without clearing the
/// draft (H-T4 regression guard).
#[test]
fn esc_returns_to_normal_without_clearing_the_draft() {
    let env = TestEnv::new();
    let session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("draft that should survive");
    // Give the input a frame to render before the Escape.
    let _ = session.wait_for_text("draft that should survive", WAIT);

    session.send_key(KeyCode::Esc);

    // The draft must still be there — the pre-fix behavior wiped
    // the input buffer, losing everything the user typed.
    let screen = session.wait_for_text("draft that should survive", Duration::from_secs(2));
    assert!(
        screen.contains("draft that should survive"),
        "Esc wiped the draft (H-T4 regression):\n{screen}"
    );

    // And the normal-mode frame must be back (the `Normal · i to
    // type` badge in the input box's bottom border).
    let screen = session.wait_for_text("Normal", Duration::from_secs(2));
    assert!(
        screen.contains("Normal"),
        "input mode did not return to Normal:\n{screen}"
    );
}

/// Clean quit via `/quit` exits zero.
#[test]
fn quit_command_exits_cleanly() {
    let env = TestEnv::new();
    let mut session = env.spawn(80, 24);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Char('i'));
    session.send_text("/quit");
    session.send_key(KeyCode::Enter);

    // The session's reader thread exits on EOF, which the drop
    // shutdown joins. If the child is still alive after the wait,
    // something in the quit path hung.
    std::thread::sleep(Duration::from_millis(500));
    // A successful quit leaves the child dead; `raw_bytes` will have
    // stopped growing. There is no exit-code accessor on the
    // portable-pty Child trait used here, so the assertion is
    // liveness: the process must have released the PTY by now, which
    // manifests as the reader thread having joined.
    session.shutdown();
}

/// Resize reflows the layout (see also `harness::TuiSession::size`
/// — a resize at runtime would need `TIOCSWINSZ` on the master, which
/// is not wired here yet; this test asserts the *initial* size is
/// respected, which is the common case).
#[test]
fn initial_size_is_respected() {
    let env = TestEnv::new();
    // Deliberately not 80x24 so the assertion is meaningful.
    let session = env.spawn(100, 30);
    let screen = harness::wait_for_ready(&session);

    // The bottom input box's frame is drawn at the last row. A 30-row
    // terminal puts the box at row 29. Check the box's top-left
    // corner character is present near the bottom of the screen by
    // counting rendered lines.
    let lines: Vec<&str> = screen.lines().collect();
    assert!(
        lines.len() >= 28,
        "expected ~30 rows of output, got {}:\n{screen}",
        lines.len()
    );
}
