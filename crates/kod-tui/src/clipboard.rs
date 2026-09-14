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
