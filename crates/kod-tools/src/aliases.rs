//! Tool-name aliases.
//!
//! Models are trained on many harnesses. A model that learned
//! `shell_exec` in one context and `run_command` in another calls the
//! tool by the name it remembers, not the name kod registered. The
//! pre-alias behavior was a `ToolNotFound` error and a wasted round
//! trip; the model usually recovers by guessing again, and sometimes
//! it does not.
//!
//! This table maps the names other harnesses use to kod's. It is a
//! *proactive* fix for the same class the reactive
//! `RetryAction::ReinjectTools` handles: instead of telling the model
//! "that tool does not exist, here are the real ones," the call just
//! works.
//!
//! Every alias maps to a name kod actually registers. An alias for a
//! tool kod does not have would be worse than none — it would convert
//! a clear error into a confusing one.

/// Resolve a model-supplied tool name to the name kod registered.
///
/// Returns the input unchanged when no alias matches, so a caller can
/// use this unconditionally.
pub fn resolve_tool_name(name: &str) -> &str {
    // Strip a `functions.` prefix, which some OpenAI-compatible
    // gateways add. Everything below operates on the bare form, and
    // every return path returns a slice of `name` or a `'static`
    // table entry — never a temporary.
    let bare = name.strip_prefix("functions.").unwrap_or(name);

    // Exact alias match.
    if let Some((_, kod)) = TABLE.iter().find(|(alias, _)| *alias == bare) {
        return kod;
    }

    // Case-insensitive alias match. `bare` is not lowercased here
    // (that would allocate); the table is all-lowercase, so compare
    // with `eq_ignore_ascii_case`.
    if let Some((_, kod)) = TABLE
        .iter()
        .find(|(alias, _)| bare.eq_ignore_ascii_case(alias))
    {
        return kod;
    }

    // Not an alias — but it may be a *real* tool name in the wrong
    // case (`Read_File`), or a prefixed real name
    // (`functions.read_file`). Both resolve to the bare form when the
    // prefix was the only thing wrong. Returning `name` when nothing
    // matched keeps a genuinely unknown tool's name intact for the
    // error message.
    if bare.len() != name.len() {
        // The prefix was stripped, so return the bare form: a caller
        // that hit a real tool should get the real name back.
        return bare;
    }
    name
}

/// `(alias, kod name)`. Aliases are lowercase; matching lowercases the
/// input, so the table only needs one case.
const TABLE: &[(&str, &str)] = &[
    // Shell execution, under the names other harnesses use.
    ("shell", "execute_command"),
    ("shell_exec", "execute_command"),
    ("run_command", "execute_command"),
    ("run_shell", "execute_command"),
    ("bash", "execute_command"),
    ("terminal", "execute_command"),
    ("exec", "execute_command"),
    // File reads.
    ("read", "read_file"),
    ("cat", "read_file"),
    ("view_file", "read_file"),
    ("open_file", "read_file"),
    ("file_read", "read_file"),
    // File writes.
    ("write", "write_file"),
    ("create_file", "write_file"),
    ("save_file", "write_file"),
    ("file_write", "write_file"),
    // Patch / edit.
    ("edit_file", "patch_file"),
    ("apply_patch", "patch_file"),
    ("str_replace", "patch_file"),
    ("str_replace_editor", "patch_file"),
    ("edit", "patch_file"),
    // Search.
    ("grep", "grep"),
    ("file_grep", "grep"),
    ("search", "grep"),
    ("ripgrep", "grep"),
    ("rg", "grep"),
    ("search_files", "grep"),
    // Directory listing.
    ("ls", "list_files"),
    ("list_dir", "list_files"),
    ("list_directory", "list_files"),
    ("dir", "list_files"),
    // File metadata.
    ("stat", "file_info"),
    ("file_stat", "file_info"),
    // Git.
    ("git_status", "git_status"),
    ("git_diff", "git_diff"),
    // Web.
    ("fetch", "web_fetch"),
    ("http_get", "web_fetch"),
    ("curl", "web_fetch"),
    // Task tracking.
    ("todo_write", "todo"),
    ("todos", "todo"),
    ("task_list", "todo"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_aliases_resolve() {
        assert_eq!(resolve_tool_name("shell_exec"), "execute_command");
        assert_eq!(resolve_tool_name("file_grep"), "grep");
        assert_eq!(resolve_tool_name("str_replace_editor"), "patch_file");
    }

    #[test]
    fn a_real_name_passes_through_unchanged() {
        assert_eq!(resolve_tool_name("read_file"), "read_file");
        assert_eq!(resolve_tool_name("execute_command"), "execute_command");
    }

    #[test]
    fn an_unknown_name_passes_through() {
        // The caller's error path is responsible for "no such tool";
        // the resolver must not invent a target.
        assert_eq!(resolve_tool_name("my_custom_tool"), "my_custom_tool");
    }

    #[test]
    fn functions_prefix_is_stripped() {
        assert_eq!(resolve_tool_name("functions.read_file"), "read_file");
        assert_eq!(resolve_tool_name("functions.shell_exec"), "execute_command");
    }

    #[test]
    fn alias_matching_is_case_insensitive() {
        // Aliases match case-insensitively. A *real* tool name in the
        // wrong case is not the alias module's job — it does not know
        // the registry's names — so it passes through and the
        // registry's case-insensitive lookup handles it.
        assert_eq!(resolve_tool_name("SHELL_EXEC"), "execute_command");
        assert_eq!(resolve_tool_name("File_Grep"), "grep");
    }

    #[test]
    fn every_alias_maps_to_a_registered_name() {
        // The table's targets must be names the engine actually
        // registers. A typo here converts a clear "no such tool" into
        // a confusing one, so the set is pinned.
        const REGISTERED: &[&str] = &[
            "read_file",
            "write_file",
            "patch_file",
            "list_files",
            "file_info",
            "grep",
            "execute_command",
            "git_status",
            "git_diff",
            "web_fetch",
            "todo",
            "batch",
            "tool_search",
        ];
        for (alias, kod) in TABLE {
            assert!(
                REGISTERED.contains(kod),
                "alias {alias:?} maps to {kod:?}, which is not a registered tool",
            );
        }
    }
}
