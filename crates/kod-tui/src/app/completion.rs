//! Tab-completion state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Owns the slash-command, model,
//! and filesystem-path completion candidate lists plus the cursor
//! into the active list. Kept separate from `input.rs` because the
//! candidates depend on external state (skills, models, cwd) while
//! `input.rs` is purely textual.

use super::*;

impl KodApp {
    /// Candidates matching the current input (only when it starts with `/`).
    /// Prefix matches rank first; subsequence (fuzzy) matches follow so
    /// `/md` still finds `/model`.
    pub fn completion_candidates(&self) -> Vec<SlashCommand> {
        if !self.input.starts_with('/') || self.input.contains(char::is_whitespace) {
            return Vec::new();
        }
        let mut prefix = Vec::new();
        let mut fuzzy = Vec::new();
        for cmd in SLASH_COMMANDS {
            if cmd.name.starts_with(&self.input) {
                prefix.push(*cmd);
            } else if fuzzy_match(cmd.name, &self.input) {
                fuzzy.push(*cmd);
            }
        }
        prefix.extend(fuzzy);
        prefix
    }

    /// Model-name completion for `/model <partial>`: static well-known
    /// names plus anything used before in this history file.
    pub fn model_candidates(&self) -> Vec<String> {
        let mut parts = self.input.split_whitespace();
        if parts.next() != Some("/model") {
            return Vec::new();
        }
        let partial = parts.next().unwrap_or("").to_lowercase();
        const KNOWN: &[&str] = &[
            "codellama:13b",
            "llama3.1",
            "llama3.1:8b",
            "qwen2.5-coder",
            "qwen2.5-coder:7b",
            "deepseek-coder-v2",
            "mistral",
            "mixtral",
            "gpt-oss:20b",
        ];
        let mut out: Vec<String> = KNOWN
            .iter()
            .filter(|m| m.to_lowercase().contains(&partial))
            .map(|m| m.to_string())
            .collect();
        for name in &self.available_models {
            if !name.is_empty() && name.to_lowercase().contains(&partial) && !out.contains(name) {
                out.push(name.clone());
            }
        }
        for entry in &self.input_history {
            if let Some(name) = entry.strip_prefix("/model ") {
                let name = name.trim().to_string();
                if !name.is_empty()
                    && name.to_lowercase().contains(&partial)
                    && !out.contains(&name)
                {
                    out.push(name);
                }
            }
        }
        out.truncate(8);
        out
    }

    /// The last input token if it looks like a path, with its byte offset.
    fn path_token(&self) -> Option<(usize, String)> {
        let (start, token) = Self::last_arg_token(&self.input)?;
        // A leading `/` at position 0 is the slash-command slot, not a path.
        if start == 0 && token.starts_with('/') {
            return None;
        }
        let stripped = token.strip_prefix('@').unwrap_or(&token);
        // Strip one layer of surrounding quotes for the shape check.
        let shape = stripped.trim_matches(|c| c == '"' || c == '\'');
        if token.starts_with('@')
            || shape.contains('/')
            || shape.starts_with('.')
            || shape.starts_with('~')
        {
            Some((start, token))
        } else {
            None
        }
    }

    /// Split off the last argument token, honoring single/double quotes so
    /// `read "my docs/rep` completes inside the quoted span. Returns the
    /// token's byte offset and its unquoted text.
    fn last_arg_token(input: &str) -> Option<(usize, String)> {
        let trimmed_end = input.trim_end_matches(' ').len();
        let input = &input[..trimmed_end];
        if input.is_empty() {
            return None;
        }
        let bytes = input.as_bytes();
        let mut in_single = false;
        let mut in_double = false;
        let mut token_start = 0;
        for (i, b) in bytes.iter().enumerate() {
            match b {
                b'\'' if !in_double => in_single = !in_single,
                b'"' if !in_single => in_double = !in_double,
                b' ' | b'\t' if !in_single && !in_double => token_start = i + 1,
                _ => {}
            }
        }
        if token_start >= input.len() {
            return None;
        }
        let raw = &input[token_start..];
        let token = raw.trim_matches(|c| c == '"' || c == '\'').to_string();
        if token.is_empty() {
            return None;
        }
        Some((token_start, token))
    }

    /// Filesystem entries matching the path token (dirs get `/` suffix).
    pub fn path_candidates(&self) -> Vec<String> {
        let (_, token) = match self.path_token() {
            Some(t) => t,
            None => return Vec::new(),
        };
        let raw = token.strip_prefix('@').unwrap_or(&token);
        let raw = Self::expand_tilde(raw);

        let (dir_part, prefix) = match raw.rfind('/') {
            Some(i) => (raw[..=i].to_string(), raw[i + 1..].to_string()),
            None => (String::new(), raw.clone()),
        };
        let dir = if dir_part.is_empty() {
            ".".to_string()
        } else {
            dir_part.clone()
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<String> = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) {
                continue;
            }
            if name.starts_with('.') && !prefix.starts_with('.') {
                continue;
            }
            let mut candidate = format!("{}{}", dir_part, name);
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                candidate.push('/');
            }
            out.push(candidate);
        }
        out.sort();
        out.truncate(10);
        out
    }

    /// Length of whichever completion list is currently active.
    pub fn active_completion_len(&self) -> usize {
        let slash = self.completion_candidates().len();
        if slash > 0 {
            slash
        } else {
            let models = self.model_candidates().len();
            if models > 0 {
                models
            } else {
                self.path_candidates().len()
            }
        }
    }

    /// Which popup list is active: slash commands, model names, or paths.
    pub fn active_completion_kind(&self) -> CompletionKind {
        if !self.completion_candidates().is_empty() {
            CompletionKind::Slash
        } else if !self.model_candidates().is_empty() {
            CompletionKind::Model
        } else if !self.path_candidates().is_empty() {
            CompletionKind::Path
        } else {
            CompletionKind::None
        }
    }

    pub fn show_completions(&self) -> bool {
        *self.input_mode() == InputMode::Insert && self.active_completion_len() > 0
    }

    pub fn completion_index(&self) -> usize {
        self.completion_index
    }

    pub fn completion_next(&mut self) {
        let n = self.active_completion_len();
        if n > 0 {
            self.completion_index = (self.completion_index + 1) % n;
        }
    }

    pub fn completion_prev(&mut self) {
        let n = self.active_completion_len();
        if n > 0 {
            self.completion_index = (self.completion_index + n - 1) % n;
        }
    }

    /// Replace the input with the selected completion (plus trailing space
    /// when the command takes an argument). Handles slash, model, and path
    /// lists depending on which popup is active.
    pub fn accept_completion(&mut self) {
        let candidates = self.completion_candidates();
        if !candidates.is_empty() {
            let selected = candidates[self.completion_index % candidates.len()];
            let needs_arg = selected.name == "/model" || selected.name == "/search";
            let next = if needs_arg {
                format!("{} ", selected.name)
            } else {
                selected.name.to_string()
            };
            self.set_input(next);
            self.completion_index = 0;
            return;
        }
        // `/model <partial>` completes the model name.
        let models = self.model_candidates();
        if !models.is_empty() {
            let selected = models[self.completion_index % models.len()].clone();
            self.set_input(format!("/model {selected}"));
            self.completion_index = 0;
            return;
        }
        // Otherwise complete the path token, keeping any `@` sigil and
        // adding a trailing space for files (dirs keep `/` so the user
        // can keep drilling down).
        let paths = self.path_candidates();
        if paths.is_empty() {
            return;
        }
        let selected = paths[self.completion_index % paths.len()].clone();
        let (start, token) = match self.path_token() {
            Some(t) => t,
            None => return,
        };
        let at = token.starts_with('@');
        let mut replacement = if at && !selected.starts_with('@') {
            format!("@{}", selected)
        } else {
            selected
        };
        if !replacement.ends_with('/') {
            replacement.push(' ');
        }
        let mut next = self.input[..start].to_string();
        next.push_str(&replacement);
        self.set_input(next);
        self.completion_index = 0;
    }

    pub fn reset_completion(&mut self) {
        self.completion_index = 0;
    }

    /// Expand a leading `~` to the home directory for path completion.
    fn expand_tilde(raw: &str) -> String {
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return format!("{}/{rest}", home.display());
            }
        } else if raw == "~"
            && let Some(home) = dirs::home_dir()
        {
            return home.display().to_string();
        }
        raw.to_string()
    }

    /// The markdown render cache. The chat widget reads it to
    /// avoid re-parsing unchanged messages on every frame; the
    /// engine does not touch it. Exposed as an `Arc` so a
    /// caller (a future sidebar widget, a test) can hold a
    /// reference without borrow-checker gymnastics.
    pub fn render_cache(&self) -> &std::sync::Arc<crate::markdown::RenderCache> {
        &self.render_cache
    }
}
