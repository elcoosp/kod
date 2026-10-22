//! `run_prompt_remote`'s daemon-facing paths.
//!
//! The "daemon" is a one-shot `tokio::net::UnixListener` on a temp
//! path. It never spawns `kod serve`, never loads a config, and
//! speaks only the frames these tests need. `run_prompt_remote` takes
//! its socket path as an argument, so no env seam is involved.
//!
//! Covered: connect failure, an `error` frame, a `done` frame, and a
//! stream that closes with neither (the mid-stream fall-through).
use kod_cli::commands::prompt::run_prompt_remote;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Bind a one-shot listener under `dir`, read exactly one request
/// line, write `frames`, then close. Returns the socket path and the
/// task handle so a test can surface a server-side panic.
///
/// The request is read before any frame is written: that ordering
/// guarantees the client's write has reached us, so the client can
/// never see EPIPE on its own write racing our close.
fn fake_daemon(
    dir: &std::path::Path,
    frames: &'static [u8],
) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let sock = dir.join("daemon.sock");
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept one client");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut request = String::new();
        reader
            .read_line(&mut request)
            .await
            .expect("read the request line");
        write_half.write_all(frames).await.expect("write frames");
        write_half.flush().await.ok();
        // Dropping both halves closes the connection.
    });
    (sock, handle)
}

/// No listener at the path: the connect is refused and the error
/// names the path and the fix (`kod serve`).
#[tokio::test]
async fn connect_refused_is_invalid_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock = tmp.path().join("absent.sock");
    let err = run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect_err("no listener at the path must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("could not connect to daemon"),
        "unexpected error: {msg}"
    );
    assert!(msg.contains("kod serve"), "missing fix hint: {msg}");
}

/// An `error` frame ends the loop and surfaces the daemon's own
/// message, not a generic exit code.
#[tokio::test]
async fn error_frame_is_surfaced() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"prompt-1\",\"type\":\"error\",\"data\":{\"message\":\"boom\"}}\n",
    );
    let err = run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect_err("an error frame must be an Err");
    server.await.expect("fake daemon must not panic");
    assert!(
        err.to_string().contains("daemon error: boom"),
        "unexpected error: {err}"
    );
}

/// A `done` frame ends the prompt cleanly.
#[tokio::test]
async fn done_frame_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"prompt-1\",\"type\":\"done\"}\n",
    );
    run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect("a done frame must end the prompt with Ok");
    server.await.expect("fake daemon must not panic");
}

/// The stream ends with neither `done` nor `error`: the daemon died
/// mid-stream, and the caller must not mistake the truncation for
/// success.
#[tokio::test]
async fn closed_stream_without_done_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(tmp.path(), b"");
    let err = run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect_err("a stream closed with no done/error frame must be an Err");
    server.await.expect("fake daemon must not panic");
    assert!(
        err.to_string()
            .contains("closed the connection before completing"),
        "unexpected error: {err}"
    );
}

/// A `chunk` frame with ordinary text: the client prints the text
/// (via `print!`) and continues reading. The `done` frame that follows
/// ends the prompt with `Ok`. The assertion is the return value; the
/// coverage is the `Some("chunk") => print!` arm, which no other test
/// in this file reaches (they only send `done` / `error` / nothing).
#[tokio::test]
async fn chunk_frames_print_and_continue() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        concat!(
            "{\"v\":1,\"id\":\"prompt-1\",\"type\":\"chunk\",\"data\":\"hello \"}\n",
            "{\"v\":1,\"id\":\"prompt-1\",\"type\":\"chunk\",\"data\":\"world\"}\n",
            "{\"v\":1,\"id\":\"prompt-1\",\"type\":\"done\"}\n",
        )
        .as_bytes(),
    );
    run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect("two chunks then done must end the prompt with Ok");
    server.await.expect("fake daemon must not panic");
}

/// A `chunk` frame carrying a `\0kod-*` control marker: the client
/// drops it (`is_control_marker` returns true, `continue`) and reads
/// the next frame. The `done` frame that follows ends with `Ok`.
///
/// The test cannot observe the print/not-print distinction without a
/// stdout capture, so the assertion is the return value; the coverage
/// is the `if is_control_marker(data) { continue; }` branch, which the
/// print test above would skip past.
#[tokio::test]
async fn chunk_with_control_marker_is_skipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        concat!(
            "{\"v\":1,\"id\":\"prompt-1\",\"type\":\"chunk\",\"data\":\"\\u0000kod-test\"}\n",
            "{\"v\":1,\"id\":\"prompt-1\",\"type\":\"done\"}\n",
        )
        .as_bytes(),
    );
    run_prompt_remote("hello".to_string(), Some(sock))
        .await
        .expect("a marker chunk then done must end the prompt with Ok");
    server.await.expect("fake daemon must not panic");
}
