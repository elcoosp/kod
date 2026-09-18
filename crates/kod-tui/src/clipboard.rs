//! Minimal cross-platform clipboard access.
//!
//! Uses `pbcopy` on macOS, `xclip`/`xsel` on Linux, and the Win32 API on
//! Windows. Each platform tries several backends so a missing one never
//! hard-fails. The whole module is a thin shim — if nothing matches,
//! `write_clipboard` returns `false` and the caller falls back to a toast.

use std::process::Command;

pub fn write_clipboard(text: &str) -> bool {
    let bytes = text.as_bytes();

    #[cfg(target_os = "macos")]
    {
        let mut child = match Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return false,
        };
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(bytes);
            let _ = stdin.flush();
        }
        return child.wait().map(|s| s.success()).unwrap_or(false);
    }

    #[cfg(target_os = "linux")]
    {
        for (bin, args) in [
            ("xclip", vec!["-sel", "clipboard", "-i"]),
            ("xsel", vec!["--clipboard", "--input"]),
        ] {
            let mut child = match Command::new(bin)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(_) => continue,
            };
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(bytes);
                let _ = stdin.flush();
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
        // No clipboard tool succeeded on this Linux host.
        return false;
    }

    #[cfg(target_os = "windows")]
    {
        // Win32 clipboard writing is not implemented yet; callers fall
        // back to a toast when this returns false.
        let _ = bytes;
        return false;
    }

    // Unreachable on every supported target (each cfg block above
    // returns). Kept so the fn still has a tail expression if a future
    // port adds an OS without adding a block — and to silence the
    // `unreachable_code` lint that fired on the previous trailing
    // `false` on macOS.
    #[allow(unreachable_code)]
    false
}

/// Read the system clipboard into a string. Mirrors
/// [`write_clipboard`]: same backends, same best-effort contract.
/// Returns `None` when the clipboard is unreachable or empty.
pub fn read_clipboard() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("pbpaste").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).into_owned();
        return Some(s);
    }

    #[cfg(target_os = "linux")]
    {
        for (bin, args) in [
            ("xclip", vec!["-sel", "clipboard", "-o"]),
            ("xsel", vec!["--clipboard", "--output"]),
            ("wl-paste", vec![]),
        ] {
            let out = match Command::new(bin).args(&args).output() {
                Ok(o) => o,
                Err(_) => continue,
            };
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
        }
        return None;
    }

    #[cfg(target_os = "windows")]
    {
        // No Win32 clipboard read implemented yet; callers fall back
        // to a message.
        return None;
    }

    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod coverage_clipboard {
    //! The clipboard module is a best-effort shim over platform
    //! tools. Its contract is "never panic, always return a
    //! `bool` or `Option`" — a regression that panics on a
    //! missing tool would take the whole TUI down on a headless
    //! machine. The tests exercise that contract without
    //! requiring a working clipboard.
    use super::*;

    #[test]
    fn write_clipboard_returns_a_bool_and_never_panics() {
        // Any environment: real terminal, headless CI, no
        // clipboard tool. The call must complete and return a
        // `bool` — never panic, never hang.
        let _ok: bool = write_clipboard("the test's content");
    }

    #[test]
    fn write_clipboard_handles_empty_input() {
        let _ok: bool = write_clipboard("");
    }

    #[test]
    fn write_clipboard_handles_multiline_content() {
        let _ok: bool = write_clipboard("line one\nline two\nline three");
    }

    #[test]
    fn write_clipboard_handles_unicode() {
        let _ok: bool = write_clipboard("café — 日本語 🚀");
    }

    #[test]
    fn read_clipboard_returns_an_option_and_never_panics() {
        // Same contract on the read side. The value may be `None`
        // on a machine with no clipboard tool; the contract is
        // that the call returns, not that it succeeds.
        let _v: Option<String> = read_clipboard();
    }

    #[test]
    fn write_then_read_round_trip_or_gracefully_degrades() {
        // On a machine with a working clipboard, a write followed
        // by a read eventually sees the same content. On one
        // without, either (or both) calls return the graceful
        // failure value. Either outcome is correct; a panic is not.
        //
        // `pbcopy` / `xclip` / `wl-paste` hand the bytes to a
        // system clipboard daemon asynchronously; the daemon makes
        // no synchronization guarantee about when the new content
        // becomes visible to the next read. The read is therefore
        // retried a bounded number of times before the assertion
        // applies, so a slow daemon under load (a coverage run is
        // the classic case) is a scheduling fact rather than a
        // test failure.
        let content = "kod-clipboard-test-unique-payload";
        if !write_clipboard(content) {
            // No clipboard tool available on this host. Nothing
            // to assert.
            return;
        }
        let mut last: Option<String> = None;
        for _ in 0..10 {
            match read_clipboard() {
                Some(got) if got.trim_end() == content => return,
                other => last = other,
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Reaching this point is a genuine inconsistency, not a
        // timing fact: either the daemon accepted the write but
        // never surfaced it (a real bug), or the read tool exists
        // but is broken (also a real bug).
        match last {
            Some(got) => panic!(
                "clipboard round trip never settled: wrote {content:?}, last read was {got:?}"
            ),
            None => panic!("clipboard write succeeded but every read returned None"),
        }
    }
}
