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
