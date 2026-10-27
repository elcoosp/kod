//! `@`-reference expansion: turns `@path/to/file` tokens in a prompt
//! into inlined file contents, and sweeps a workspace's background
//! spool directory. Extracted from `engine/mod.rs`. Free functions,
//! no engine state.

/// Expand `@path` references in `input` into fenced code blocks
/// containing the referenced file's content.
///
/// This runs before the prompt reaches the router. A reference is an
/// `@` at a word boundary followed by a path-shaped token: no
/// whitespace, and containing a `/`, a `.`, or ending at the end of
/// input. Paths are resolved against `working_dir`; `~/` expands to the
/// home directory. The file is inserted as
/// `\n\n<file path=\"...\">\n...\n</file>\n\n` so the model sees it as
/// an explicit, named context block rather than text woven into the
/// question.
///
/// Errors are silent: a non-existent path or an unreadable file leaves
/// the `@path` token untouched. A user who typed `@nonexistent` gets
/// their literal input back; the model handles the ambiguity naturally.
/// A user who typed `@real/file.rs` and got content back does not need
/// to know about the error path.
///
/// Reads cap at [`MAX_AT_REF_BYTES`] per file; a file larger than that
/// is truncated with a marker. The total number of files per prompt
/// caps at [`MAX_AT_REFS`] so a user who pastes a wall of @-tokens
/// cannot blow the context window on one turn.
pub fn expand_at_references(input: &str, working_dir: &std::path::Path) -> String {
    expand_at_references_with_policy(input, working_dir, None)
}

/// Same as [`expand_at_references`], but honours
/// `[policy.read_protection]`.
///
/// `read_file` and `grep` refuse a `Refuse`-mode match and redact a
/// `Redact`-mode one; this expander read every referenced file and
/// pasted it into the prompt with only a containment check. The prompt
/// is model-visible, so a `@.env` token shipped the file verbatim —
/// and the `@`-tokens are not only user-typed: `process.rs` expands the
/// Jev-clarified input too.
pub fn expand_at_references_with_policy(
    input: &str,
    working_dir: &std::path::Path,
    protection: Option<&kod_config::ReadProtection>,
) -> String {
    const MAX_AT_REF_BYTES: usize = 64 * 1024;
    const MAX_AT_REFS: usize = 10;

    let mut out = String::with_capacity(input.len() + 256);
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut inserted = 0usize;
    while i < bytes.len() {
        // An @ starts a reference only at a word boundary: previous
        // byte must be whitespace or start of input.
        let at_word_start = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if bytes[i] == b'@' && at_word_start {
            // Scan the token: everything up to whitespace.
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let token = &input[i + 1..j];
            // Heuristic for "looks like a path": non-empty, and
            // contains `/`, `.`, or `~`. This filters out `@user`
            // mentions that are not paths.
            let looks_like_path = !token.is_empty()
                && (token.contains('/') || token.contains('.') || token.starts_with('~'));
            if looks_like_path && inserted < MAX_AT_REFS {
                let expanded = expand_one_at_ref(token, working_dir, MAX_AT_REF_BYTES, protection);
                if let Some(text) = expanded {
                    out.push_str(&text);
                    inserted += 1;
                    i = j;
                    continue;
                }
            }
        }
        // Copy the byte through unchanged. Multi-byte UTF-8 preserves
        // because we copy byte-by-byte and the input was valid UTF-8.
        let ch = input[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    // Re-decode as UTF-8 — the byte-copy above yields a valid string
    // because we only skipped whole bytes when expanding.
    out
}

/// F2c-9: keep `~/.kod/background/` from growing without bound. The
/// spool files are per-job output captures; a session that runs many
/// background commands leaves one file each, never removed. Sweep to
/// the newest `MAX` by mtime when a new job starts (cheap: one
/// `read_dir` per job creation, not per frame).
///
/// Best-effort: a filesystem error is logged and ignored — a full
/// sweep is a hygiene nicety, not a correctness path.
pub(crate) fn sweep_background_spools(dir: &std::path::Path) {
    const MAX_SPOOLS: usize = 64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("log") {
                return None;
            }
            let t = e.metadata().ok()?.modified().ok()?;
            Some((p, t))
        })
        .collect();
    if files.len() <= MAX_SPOOLS {
        return;
    }
    // Oldest first; remove everything past the newest MAX.
    files.sort_by_key(|(_, t)| *t);
    let remove = files.len() - MAX_SPOOLS;
    for (p, _) in files.into_iter().take(remove) {
        let _ = std::fs::remove_file(&p);
    }
}

/// Try to expand one `@path` token. Returns the fenced block on
/// success, `None` when the file cannot be read or the path is not
/// inside `working_dir` (a symlink escape is refused, matching the
/// tools' own containment check).
fn expand_one_at_ref(
    token: &str,
    working_dir: &std::path::Path,
    max_bytes: usize,
    protection: Option<&kod_config::ReadProtection>,
) -> Option<String> {
    let expanded_tilde = if let Some(rest) = token.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        home.join(rest)
    } else {
        std::path::PathBuf::from(token)
    };
    let candidate = if expanded_tilde.is_absolute() {
        expanded_tilde
    } else {
        working_dir.join(expanded_tilde)
    };
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    // Containment: the resolved target must live inside the
    // canonicalized working directory. This matches the tool-context
    // rule; without it, `@../../etc/passwd` would leak.
    let root = std::fs::canonicalize(working_dir).ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    if !canonical.is_file() {
        return None;
    }
    // `[policy.read_protection]`: refuse a `Refuse`-mode match, and
    // redact a `Redact`-mode one before it enters the prompt. The
    // prompt is model-visible, so shipping the raw file would undo the
    // policy the user set.
    if let Some(rp) = protection
        && rp.matches(&canonical)
        && rp.mode == kod_config::ReadMode::Refuse
    {
        return None;
    }
    let text = std::fs::read_to_string(&canonical).ok()?;
    let text = match protection {
        Some(rp) if rp.matches(&canonical) && rp.enabled => {
            // No redactor handle exists on this path (the expander is a
            // free function), so a `Redact`-mode file is stripped by a
            // fresh `Redactor::default()`. The default rule set is what
            // `read_file` uses when the engine has none installed.
            kod_types::redact::Redactor::default().redact(&text).0
        }
        _ => text,
    };
    let (body, truncated) = if text.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        (text[..end].to_string(), true)
    } else {
        (text, false)
    };
    let notice = if truncated {
        format!("\n[truncated at {} bytes]", max_bytes)
    } else {
        String::new()
    };
    Some(format!(
        "\n<file path=\"{}\">\n{}{}\n</file>\n",
        canonical.display(),
        body,
        notice,
    ))
}

#[cfg(test)]
mod read_protection_tests {
    //! The expander pastes file content into the prompt, which the
    //! model reads. `read_file` and `grep` both honour
    //! `[policy.read_protection]`; this path did not, so a `@.env`
    //! token shipped the file verbatim.

    use super::*;

    fn rp(mode: kod_config::ReadMode, deny: &[&str]) -> kod_config::ReadProtection {
        kod_config::ReadProtection {
            enabled: true,
            mode,
            deny: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn refuse_mode_does_not_expand_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("id_rsa.pem"), "-----BEGIN KEY-----\n").unwrap();
        let policy = rp(kod_config::ReadMode::Refuse, &["**/*.pem"]);

        let out =
            expand_at_references_with_policy("see @id_rsa.pem here", tmp.path(), Some(&policy));
        assert!(
            !out.contains("BEGIN KEY"),
            "a Refuse-mode file must not be inlined; got {out:?}",
        );
    }

    #[test]
    fn redact_mode_inlines_only_the_redacted_form() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("creds.env"),
            "OPENAI_API_KEY=sk-abcdef0123456789abcdef0123456789\n",
        )
        .unwrap();
        let policy = rp(kod_config::ReadMode::Redact, &["**/*.env"]);

        let out =
            expand_at_references_with_policy("see @creds.env here", tmp.path(), Some(&policy));
        assert!(
            !out.contains("sk-abcdef0123456789"),
            "the secret must be redacted before it enters the prompt; got {out:?}",
        );
        assert!(
            out.contains("[REDACTED:openai-key]"),
            "the marker should be present; got {out:?}",
        );
    }

    #[test]
    fn an_unprotected_file_still_expands() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "ordinary\n").unwrap();
        let policy = rp(kod_config::ReadMode::Redact, &["**/*.env"]);

        let out =
            expand_at_references_with_policy("see @notes.txt here", tmp.path(), Some(&policy));
        assert!(out.contains("ordinary"), "got {out:?}");
    }

    #[test]
    fn no_policy_keeps_the_old_behaviour() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "ordinary\n").unwrap();
        let out = expand_at_references("see @notes.txt here", tmp.path());
        assert!(out.contains("ordinary"), "got {out:?}");
    }
}
