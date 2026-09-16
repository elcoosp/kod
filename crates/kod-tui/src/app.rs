//! Application state for the TUI.
//!
//! Manages messages, input, agent status, tool execution state,
//! and scrolling.

use crate::theme::Theme;
use chrono::{DateTime, Utc};
use kod_types::{MessageId, MessageMetadata, MessageRole};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Message displayed in the chat
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: MessageMetadata,
    /// Monotonic insertion order — chat sorts by this, never by wall-clock
    /// timestamp (which can collide or go backwards after a restore).
    #[serde(default)]
    pub sequence: u64,
}

/// Agent status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub capabilities: Vec<String>,
    pub status: String,
    pub current_task: Option<String>,
}

/// A live swarm agent's chat row: the message that carries its
/// progress and the running/finished flag. The message id is stable for
/// the agent's lifetime so chunks append in place rather than spawning
/// a new row per token.
#[derive(Debug, Clone)]
pub struct SwarmAgentView {
    pub message_id: MessageId,
    pub finished: bool,
}

/// Tool execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub tool_name: String,
    pub status: ToolStatus,
    pub start_time: DateTime<Utc>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

/// Application modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    AgentPanel,
    ToolExecution,
    Help,
    Input,
}

/// Input modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
}

/// A slash command the TUI understands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub hint: &'static str,
}

/// Commands available through `/` autocomplete in the input box
pub const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        hint: "show available commands",
    },
    SlashCommand {
        name: "/clear",
        hint: "clear chat history",
    },
    SlashCommand {
        name: "/model",
        hint: "switch model: /model <name>",
    },
    SlashCommand {
        name: "/skills",
        hint: "list loaded skills",
    },
    SlashCommand {
        name: "/goal",
        hint: "set a goal the agent works toward: /goal <text> | /goal clear",
    },
    SlashCommand {
        name: "/steer",
        hint: "redirect the running prompt: /steer <instruction>",
    },
    SlashCommand {
        name: "/cancel",
        hint: "stop the running prompt",
    },
    SlashCommand {
        name: "/compact",
        hint: "compact session history now",
    },
    SlashCommand {
        name: "/swarm",
        hint: "run N agents on a goal: /swarm <goal>",
    },
    SlashCommand {
        name: "/quit",
        hint: "quit kod",
    },
    SlashCommand {
        name: "/undo",
        hint: "restore chat cleared with /clear",
    },
    SlashCommand {
        name: "/edit",
        hint: "edit your last message again",
    },
    SlashCommand {
        name: "/search",
        hint: "search chat: /search <text> (n/N jumps)",
    },
    SlashCommand {
        name: "/theme",
        hint: "switch theme: /theme [dark|light]",
    },
    SlashCommand {
        name: "/tools",
        hint: "toggle tool output details",
    },
    SlashCommand {
        name: "/retry",
        hint: "reconnect + resend the last prompt",
    },
    SlashCommand {
        name: "/copy",
        hint: "copy the last assistant reply",
    },
    SlashCommand {
        name: "/debug",
        hint: "diagnostics: /debug last-prompt dumps the last prompt",
    },
    SlashCommand {
        name: "/rollback",
        hint: "restore a file from a checkpoint: /rollback [id]",
    },
    SlashCommand {
        name: "/checkpoints",
        hint: "list file checkpoints for this project",
    },
];

/// What the generation is currently doing — shown in the header/status so
/// the spinner is never a generic "thinking…" mystery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenPhase {
    Idle,
    Connecting,
    Generating,
    ExecutingTool(String),
    Summarizing,
}

/// Destructive action awaiting a yes/no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Clear,
    Quit,
}

/// Spinner frames for the "thinking" indicator
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Main application state
pub struct KodApp {
    mode: AppMode,
    input_mode: InputMode,
    input: String,
    cursor_position: usize,
    input_history: Vec<String>,
    history_index: Option<usize>,
    /// Unsent draft stashed when the user first presses Up — Down past the
    /// newest entry restores it instead of blanking the box.
    draft: String,

    messages: Vec<Message>,
    /// Display rows scrolled back; 0 = pinned to live bottom.
    /// Rows (not logical lines): long lines wrap, and the chat widget
    /// counts wrapped rows so the newest content stays reachable.
    scroll_lines: usize,

    agents: HashMap<String, AgentInfo>,
    tool_executions: Vec<ToolExecution>,
    current_tool: Option<String>,

    current_response: String,
    is_streaming: bool,
    /// True once this generation flushed streamed text into a bubble
    /// (tool interleaving). Lets `finish_response` tell "nothing streamed"
    /// apart from "already shown" so it doesn't reprint the full reply.
    stream_flushed_bubble: bool,

    /// True from submit until the response (or error) lands
    generating: bool,
    spinner_frame: usize,
    /// When the current generation started; drives the time-based spinner
    /// so it spins with every render even when chunk events starve ticks.
    spinner_started: Option<Instant>,
    model_name: String,
    completion_index: usize,
    /// Model names the provider reported (for `/model` completion).
    available_models: Vec<String>,

    /// Rough session token accounting (1 token ≈ 4 chars over prompts +
    /// replies). Drives the context meter and auto-compact.
    context_tokens: usize,
    /// Set when the provider has reported real token usage for the
    /// current turn. Once set, the char-based estimate stops
    /// contributing to `context_tokens` until the next
    /// `begin_generation`.
    ///
    /// The provider's `usage` totals are authoritative and already
    /// include every token the model saw (prompt + completion). The
    /// estimate that `note_usage` accumulates from
    /// `note_prompt` / `flush_streamed_text` / `finish_response` also
    /// runs across the same turn. Without this flag the two stacked:
    /// TokenUsage arrived, replaced the estimate with the real total,
    /// and then finish_response immediately added the assistant's
    /// chars/4 on top — over-reporting the context by roughly the
    /// reply length every turn.
    turn_has_real_usage: bool,
    context_limit: usize,
    compacted_messages: usize,

    /// Skills loaded at startup (names only, for `/skills`).
    loaded_skills: Vec<String>,

    /// Active goal set via `/goal` (see [`KodApp::set_goal`]).
    goal: Option<String>,

    /// Stack of chats wiped by `/clear` — `/undo` restores the newest.
    cleared_stack: Vec<Vec<Message>>,
    /// Destructive action waiting for y/n.
    pending_confirm: Option<ConfirmKind>,

    /// Active chat search. `Some(q)` means the search bar has state;
    /// `q` may be empty (the bar is open, the user has not typed yet).
    /// See `search_editing` for the "typing into the bar" flag.
    search_query: Option<String>,
    search_index: usize,
    /// True while the search bar is open and the user is typing into
    /// it: every printable character goes into the query instead of
    /// the input box, Backspace edits the query, and Enter commits
    /// (drops `search_editing`, leaving `search_query` in place so
    /// `n`/`N` navigate). Escape always clears the search outright.
    ///
    /// Separate from `search_query.is_some()` because a committed
    /// search also has a query but is not being edited — the
    /// distinction is what lets `n` append a character to a query
    /// being typed and mean "next match" once typing is done.
    search_editing: bool,

    /// Tool message ids unfolded to their full output (Enter toggles).
    expanded_tools: HashSet<MessageId>,
    /// Hide tool rows entirely for a cleaner view (`/tools`, `t`).
    show_tools: bool,

    /// Current generation phase + when it started (elapsed display).
    phase: GenPhase,
    /// Last prompt sent (powers `/retry` after a failure).
    last_prompt: Option<String>,
    /// Consecutive generation failures (offline indicator + backoff hint).
    fail_count: usize,
    /// Active color theme.
    theme: Theme,
    /// Full-screen help overlay (`?`, `/help`, F1).
    show_help: bool,
    /// Last generation error, kept visible in the status bar until the next
    /// prompt starts. Chat also gets the friendly (actionable) version.
    last_error: Option<String>,

    /// Live swarm-agent views, keyed by agent id. Cleared at the
    /// start of each swarm run; a running agent appends its chunks to
    /// the chat message whose id is stored here.
    swarm_agents: std::collections::HashMap<kod_types::AgentId, SwarmAgentView>,
    /// Wall-clock instant the session started.
    session_started_at: Instant,
    /// Accumulated input tokens the provider has reported this session.
    /// Distinct from `context_tokens` (which is a window snapshot and
    /// shrinks under compaction); this counter only grows.
    session_input_tokens: usize,
    /// Accumulated output tokens the provider has reported this session.
    session_output_tokens: usize,
    should_quit: bool,
    next_seq: u64,
}

/// Default context window assumed when the provider reports none.
pub const DEFAULT_CONTEXT_LIMIT: usize = 128_000;
/// Fraction of the window that triggers auto-compact.
pub const COMPACT_AT_FRACTION_NUM: usize = 4;
pub const COMPACT_AT_FRACTION_DEN: usize = 5;

impl KodApp {
    pub fn new() -> Self {
        Self {
            mode: AppMode::Normal,
            input_mode: InputMode::Normal,
            input: String::new(),
            cursor_position: 0,
            input_history: Vec::new(),
            history_index: None,
            draft: String::new(),

            messages: Vec::new(),
            scroll_lines: 0,

            agents: HashMap::new(),
            tool_executions: Vec::new(),
            current_tool: None,

            current_response: String::new(),
            is_streaming: false,
            stream_flushed_bubble: false,

            generating: false,
            spinner_frame: 0,
            spinner_started: None,
            model_name: String::new(),
            completion_index: 0,
            available_models: Vec::new(),

            context_tokens: 0,
            turn_has_real_usage: false,
            context_limit: DEFAULT_CONTEXT_LIMIT,
            compacted_messages: 0,
            loaded_skills: Vec::new(),
            goal: None,
            cleared_stack: Vec::new(),
            pending_confirm: None,
            search_query: None,
            search_index: 0,
            search_editing: false,
            expanded_tools: HashSet::new(),
            show_tools: true,
            phase: GenPhase::Idle,
            last_prompt: None,
            fail_count: 0,
            theme: Theme::dark(),
            show_help: false,
            last_error: None,

            swarm_agents: std::collections::HashMap::new(),
            session_started_at: Instant::now(),
            session_input_tokens: 0,
            session_output_tokens: 0,
            should_quit: false,
            next_seq: 0,
        }
    }

    // Mode management
    pub fn mode(&self) -> &AppMode {
        &self.mode
    }

    pub fn set_mode(&mut self, mode: AppMode) {
        self.mode = mode;
    }

    pub fn input_mode(&self) -> &InputMode {
        &self.input_mode
    }

    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
    }

    // Input handling
    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.cursor_position = self.input.len();
    }

    pub fn add_char(&mut self, c: char) {
        self.input.insert(self.cursor_position, c);
        self.cursor_position += c.len_utf8();
    }

    /// Insert a newline at the cursor (Ctrl+J / Alt+Enter in insert mode).
    /// Enter still submits — multiline never traps the user.
    pub fn insert_newline(&mut self) {
        self.add_char('\n');
    }

    pub fn cursor_position(&self) -> usize {
        self.cursor_position
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            // Step back one full char, never into the middle of UTF-8.
            let mut pos = self.cursor_position - 1;
            while pos > 0 && !self.input.is_char_boundary(pos) {
                pos -= 1;
            }
            self.cursor_position = pos;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_position < self.input.len() {
            let mut pos = self.cursor_position + 1;
            while pos < self.input.len() && !self.input.is_char_boundary(pos) {
                pos += 1;
            }
            self.cursor_position = pos;
        }
    }

    /// Delete the word before the cursor (Ctrl+W).
    pub fn delete_word_before(&mut self) {
        if self.cursor_position == 0 {
            return;
        }
        let mut start = self.cursor_position;
        let bytes = self.input.as_bytes();
        while start > 0 && bytes[start - 1] == b' ' {
            start -= 1;
        }
        while start > 0 && bytes[start - 1] != b' ' && bytes[start - 1] != b'\n' {
            start -= 1;
        }
        self.input.drain(start..self.cursor_position);
        self.cursor_position = start;
    }

    /// Delete everything from the cursor to the start of its line (Ctrl+U).
    pub fn delete_to_line_start(&mut self) {
        let line_start = self.input[..self.cursor_position]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input.drain(line_start..self.cursor_position);
        self.cursor_position = line_start;
    }

    /// (line, col) of the cursor for multiline rendering.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.input[..self.cursor_position.min(self.input.len())];
        let line = before.chars().filter(|c| *c == '\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        (line, col)
    }

    /// Rows the input box needs (header + content, clamped for layout).
    pub fn input_height_rows(&self) -> u16 {
        let lines = self.input.lines().count().max(1);
        // +2 for the box border.
        (lines as u16 + 2).clamp(3, 7)
    }

    pub fn is_multiline_input(&self) -> bool {
        self.input.contains('\n')
    }

    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.move_cursor_left();
            self.input.remove(self.cursor_position);
        }
    }

    /// Delete the character under the cursor (Delete key), leaving
    /// `cursor_position` where it was.
    ///
    /// The TUI used to route the Delete key through `set_input`, which
    /// resets `cursor_position` to `input.len()`. That was observable:
    /// placing the cursor mid-word and pressing Delete jumped the caret
    /// to the end of the line. This method edits in place like
    /// `backspace` does, and walks forward to the next char boundary so
    /// a non-ASCII character is removed whole.
    pub fn delete_at_cursor(&mut self) {
        let pos = self.cursor_position;
        if pos >= self.input.len() {
            return;
        }
        let mut end = pos + 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.input.drain(pos..end);
    }

    /// Remove the message currently being edited from history tracking so
    /// Up/Down starts over (used after `/edit` loads an old message).
    pub fn reset_history_index(&mut self) {
        self.history_index = None;
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    pub fn submit_input(&mut self) {
        if !self.input.is_empty() {
            self.input_history.push(self.input.clone());

            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::User,
                content: self.input.clone(),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            });

            self.clear_input();
            self.history_index = None;
        }
    }

    pub fn input_history(&self) -> &[String] {
        &self.input_history
    }

    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {
                self.draft = self.input.clone();
                self.history_index = Some(self.input_history.len() - 1);
                self.set_input(self.input_history[self.input_history.len() - 1].clone());
            }
            Some(0) => {}
            Some(index) => {
                self.history_index = Some(index - 1);
                self.set_input(self.input_history[index - 1].clone());
            }
        }
    }

    pub fn history_next(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {}
            Some(index) if index >= self.input_history.len() - 1 => {
                self.history_index = None;
                // Restore the stashed draft, not a blank box.
                let draft = std::mem::take(&mut self.draft);
                self.set_input(draft);
            }
            Some(index) => {
                self.history_index = Some(index + 1);
                self.set_input(self.input_history[index + 1].clone());
            }
        }
    }

    /// Load your last user message back into the input box for editing.
    /// Returns false when there is nothing to edit.
    pub fn edit_last_message(&mut self) -> bool {
        let last = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.clone());
        match last {
            Some(text) => {
                self.set_input(text);
                self.reset_history_index();
                true
            }
            None => false,
        }
    }

    // Message management
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn add_message(&mut self, message: Message) {
        // Sticky bottom: pin to live only if the user was already there.
        // Otherwise a new arrival mid-read would yank the viewport away.
        let pinned = self.scroll_lines == 0;
        let mut message = message;
        message.sequence = self.next_seq;
        self.next_seq += 1;
        self.messages.push(message);
        if pinned {
            self.scroll_to_bottom();
        }
    }

    pub fn clear_messages(&mut self) {
        if !self.messages.is_empty() {
            self.cleared_stack.push(std::mem::take(&mut self.messages));
            if self.cleared_stack.len() > 5 {
                self.cleared_stack.remove(0);
            }
        }
        self.scroll_lines = 0;
        self.expanded_tools.clear();
        self.clear_search();

        // Reset context accounting. `/clear` wipes the display AND the
        // engine's transcript (see the ConfirmKind::Clear handler in
        // main_loop, which calls engine.clear_history()), so the
        // session really is starting over. Leaving the previous
        // session's accumulated `context_tokens` in place meant the
        // next N messages inherited a count that included messages
        // the user had thrown away: the header's "≈ ctx X/Y" meter
        // overstated by the discarded amount, and `maybe_compact`'s
        // threshold — a fraction of the model window — was compared
        // against a number that no longer reflected anything.
        //
        // `compacted_messages` (the session's running total) is reset
        // for the same reason: it is meant to say "N messages have
        // been compacted *in this session*", not "since the process
        // started".
        self.context_tokens = 0;
        self.compacted_messages = 0;
    }

    /// Restore the most recently cleared chat. False when nothing is stashed.
    pub fn undo_clear(&mut self) -> bool {
        match self.cleared_stack.pop() {
            Some(msgs) => {
                self.messages = msgs;
                self.scroll_to_bottom();
                true
            }
            None => false,
        }
    }

    pub fn has_undo(&self) -> bool {
        !self.cleared_stack.is_empty()
    }

    // Pending confirmations (destructive actions)

    pub fn request_confirm(&mut self, kind: ConfirmKind) {
        self.pending_confirm = Some(kind);
    }

    pub fn pending_confirm(&self) -> Option<ConfirmKind> {
        self.pending_confirm
    }

    pub fn resolve_confirm(&mut self, confirmed: bool) -> Option<ConfirmKind> {
        let kind = self.pending_confirm.take()?;
        if confirmed {
            match kind {
                ConfirmKind::Clear => self.clear_messages(),
                ConfirmKind::Quit => self.should_quit = true,
            }
        }
        Some(kind)
    }

    // Chat search

    /// Start (or replace) a case-insensitive search; returns match count.
    pub fn set_search(&mut self, query: &str) -> usize {
        let q = query.trim();
        if q.is_empty() {
            self.clear_search();
            return 0;
        }
        self.search_query = Some(q.to_string());
        self.search_index = 0;
        let n = self.search_matches().len();
        if n > 0 {
            self.jump_to_search_match(0);
        }
        n
    }

    pub fn clear_search(&mut self) {
        self.search_query = None;
        self.search_index = 0;
        self.search_editing = false;
    }

    pub fn search_query(&self) -> Option<&str> {
        self.search_query.as_deref()
    }

    /// Indices of messages containing the query (case-insensitive).
    pub fn search_matches(&self) -> Vec<usize> {
        let q = match &self.search_query {
            Some(q) => q.to_lowercase(),
            None => return Vec::new(),
        };
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.content.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn jump_to_search_match(&mut self, _pos: usize) {
        // Pin to live bottom; the match highlight carries the position.
        // (Viewport math lives in the chat widget, which centers matches
        // when a search is active.)
        self.scroll_to_bottom();
    }

    /// Step to the next match (wraps). Returns (position, total).
    pub fn search_next(&mut self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            return None;
        }
        self.search_index = (self.search_index + 1) % n;
        self.jump_to_search_match(self.search_index);
        Some((self.search_index + 1, n))
    }

    pub fn search_prev(&mut self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            return None;
        }
        self.search_index = (self.search_index + n - 1) % n;
        self.jump_to_search_match(self.search_index);
        Some((self.search_index + 1, n))
    }

    pub fn current_search_pos(&self) -> Option<(usize, usize)> {
        let n = self.search_matches().len();
        if n == 0 {
            None
        } else {
            Some((self.search_index % n + 1, n))
        }
    }

    // Tool output expand / hide

    pub fn toggle_tool_expanded(&mut self, id: &MessageId) -> bool {
        if self.expanded_tools.remove(id) {
            false
        } else {
            self.expanded_tools.insert(id.clone());
            true
        }
    }

    pub fn is_tool_expanded(&self, id: &MessageId) -> bool {
        self.expanded_tools.contains(id)
    }

    pub fn show_tools(&self) -> bool {
        self.show_tools
    }

    pub fn toggle_show_tools(&mut self) -> bool {
        self.show_tools = !self.show_tools;
        self.show_tools
    }

    // Generation phases, retry state, themes

    pub fn phase(&self) -> &GenPhase {
        &self.phase
    }

    // NOTE: the previous `goal_progress() -> Option<(usize, usize)>`
    // returned a fixed `Some((0, 1))` whenever a goal was set, because
    // the goal loop runs entirely inside `KodEngine::process_goal_
    // streaming` and does not report per-turn progress back to the
    // TUI. Rather than keep a fake counter, the header now renders the
    // goal text (see `KodApp::goal`). If per-turn progress is ever
    // plumbed through, this method should return real numbers.

    fn set_phase(&mut self, phase: GenPhase) {
        self.phase = phase;
    }

    /// Short human label: `connecting`, `generating`, `tool: cargo test`, `thinking`.
    /// No timers — elapsed was noisy and duplicated across header/status.
    pub fn phase_label(&self) -> Option<String> {
        if !self.generating {
            return None;
        }
        let label = match &self.phase {
            GenPhase::Idle | GenPhase::Connecting => "connecting…".to_string(),
            GenPhase::Generating => "thinking…".to_string(),
            GenPhase::ExecutingTool(t) => {
                let short = t.chars().take(28).collect::<String>();
                format!("tool: {short}")
            }
            GenPhase::Summarizing => "thinking…".to_string(),
        };
        Some(label)
    }

    /// Enter the post-tool thinking phase (tool result reinjected, LLM
    /// is reasoning again). Shows "thinking…" not the stale tool line.
    pub fn begin_thinking(&mut self) {
        if self.generating {
            self.current_tool = None;
            self.set_phase(GenPhase::Summarizing);
        }
    }

    pub fn last_prompt(&self) -> Option<&str> {
        self.last_prompt.as_deref()
    }

    pub fn set_last_prompt(&mut self, prompt: &str) {
        self.last_prompt = Some(prompt.to_string());
    }

    /// True when recent generations keep failing (drives the offline badge).
    pub fn is_offline(&self) -> bool {
        self.fail_count >= 2
    }

    pub fn fail_count(&self) -> usize {
        self.fail_count
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Set the theme by name, returning `true` if the name matches a
    /// built-in theme (currently `dark` and `light`) and `false`
    /// otherwise. On `false` the theme is set to dark — the same
    /// fallback `Theme::from_name` has always applied — but the caller
    /// can now tell the user that the name was not recognized instead
    /// of printing a transition into a theme that does not exist.
    ///
    /// Previously, `/theme neon` printed "Theme dark → neon" while the
    /// palette was in fact dark; a small lie, but the whole point of
    /// a theme command is to see what you typed take effect.
    pub fn try_set_theme(&mut self, name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "dark" => {
                self.theme = Theme::dark();
                true
            }
            "light" => {
                self.theme = Theme::light();
                true
            }
            _ => {
                self.theme = Theme::dark();
                false
            }
        }
    }

    /// Cycle dark → light → dark. Returns the new theme name.
    pub fn cycle_theme(&mut self) -> String {
        let next = if self.theme.name == "light" {
            Theme::dark()
        } else {
            Theme::light()
        };
        let name = next.name.clone();
        self.theme = next;
        name
    }

    /// Context meter with teeth: warns past 70%, alarms past 90%.
    pub fn context_warning(&self) -> Option<&'static str> {
        let pct = self.context_tokens * 100 / self.context_limit.max(1);
        if pct >= 90 {
            Some("context almost full — /compact soon")
        } else if pct >= 70 {
            Some("context filling up")
        } else {
            None
        }
    }

    pub fn push_assistant_message(&mut self, content: &str) {
        // LLMs (and streamed chunks) often start with blank lines — even
        // whitespace-only ones (`"\n  \nHello"`). A raw push renders those
        // as an empty first row inside the bubble. Collapse leading and
        // trailing blank lines, keep interior formatting intact.
        let trimmed = Self::trim_blank_lines(content);
        if trimmed.is_empty() {
            return;
        }
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: trimmed.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Strip leading/trailing blank lines (whitespace-only counts as blank),
    /// preserving interior formatting.
    pub(crate) fn trim_blank_lines(s: &str) -> String {
        let lines: Vec<&str> = s.lines().collect();
        let mut start = 0;
        while start < lines.len() && lines[start].trim().is_empty() {
            start += 1;
        }
        let mut end = lines.len();
        while end > start && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        lines[start..end].join("\n")
    }

    pub fn push_system_message(&mut self, content: &str) {
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Scroll back (look at older lines)
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_lines = self.scroll_lines.saturating_add(lines);
    }

    /// Scroll forward (toward live bottom)
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_lines = self.scroll_lines.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_lines = 0;
    }

    /// Jump to the oldest content (render clamps to the real maximum).
    pub fn scroll_to_top(&mut self) {
        self.scroll_lines = usize::MAX;
    }

    pub fn is_scrolled_to_bottom(&self) -> bool {
        self.scroll_lines == 0
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_lines
    }

    // Agent management
    pub fn agents(&self) -> Vec<&AgentInfo> {
        self.agents.values().collect()
    }

    pub fn agents_map(&self) -> &HashMap<String, AgentInfo> {
        &self.agents
    }

    pub fn add_agent(&mut self, name: &str, capabilities: Vec<String>) {
        self.agents.insert(
            name.to_string(),
            AgentInfo {
                name: name.to_string(),
                capabilities,
                status: "idle".to_string(),
                current_task: None,
            },
        );
    }

    pub fn get_agent(&self, name: &str) -> Option<&AgentInfo> {
        self.agents.get(name)
    }

    pub fn update_agent_status(&mut self, name: &str, status: &str) {
        if let Some(agent) = self.agents.get_mut(name) {
            agent.status = status.to_string();
        }
    }

    pub fn update_agent_task(&mut self, name: &str, task: &str) {
        if let Some(agent) = self.agents.get_mut(name) {
            agent.current_task = Some(task.to_string());
        }
    }

    // Tool execution
    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
    }

    pub fn tool_executions(&self) -> &[ToolExecution] {
        &self.tool_executions
    }

    pub fn start_tool_execution(&mut self, tool_name: &str) {
        self.current_tool = Some(tool_name.to_string());
        self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));
        self.tool_executions.push(ToolExecution {
            tool_name: tool_name.to_string(),
            status: ToolStatus::Running,
            start_time: Utc::now(),
            result: None,
        });
        // Stream the row live: it lands in position now with a running
        // body, and completion fills that same row in — the call never
        // arrives as a block at task end.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{tool_name}]\n{}", Self::LIVE_TOOL_BODY_PLACEHOLDER),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Placeholder body of a tool row while its call is still running.
    /// Completion (or cancel/fail) replaces it in place.
    const LIVE_TOOL_BODY_PLACEHOLDER: &'static str = "running…";

    /// Index of the most recent tool row still awaiting its result.
    fn live_tool_msg(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("").trim();
            body == Self::LIVE_TOOL_BODY_PLACEHOLDER
        })
    }

    /// Refresh the live "running …" line with a one-line excerpt of what
    /// the tool is actually doing (`execute_command cargo test …`).
    /// Arrives from the engine after the call's arguments are assembled.
    pub fn update_tool_status(&mut self, display: &str) {
        let display = display.trim();
        if display.is_empty() {
            return;
        }
        self.current_tool = Some(display.to_string());
        self.set_phase(GenPhase::ExecutingTool(display.to_string()));
        // Refresh the live row's header too so the streamed row shows what
        // the call actually does, not just the tool name.
        if let Some(i) = self.live_tool_msg() {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or(Self::LIVE_TOOL_BODY_PLACEHOLDER);
            let body = body.to_string();
            self.messages[i].content = format!("[{display}]\n{body}");
        }
    }

    pub fn complete_tool_execution(&mut self, tool_name: &str, result: &str) {
        self.complete_tool_execution_with_duration(tool_name, result, None);
    }

    /// Fill the live tool row with its result, stamping the header with
    /// wall time when `duration_ms` is present (`header · 1.2s`).
    ///
    /// Idempotent: when no `Running` entry matches `tool_name` and a
    /// finished row for it already exists, this is the task-end fallback
    /// arriving after the live done-marker — keep the live (timed) row
    /// instead of rewriting it without the duration.
    pub fn complete_tool_execution_with_duration(
        &mut self,
        tool_name: &str,
        result: &str,
        duration_ms: Option<u64>,
    ) {
        // `tool_name` here is usually the rendered header
        // (`execute_command command=…`), not the plain name stored at
        // start time — match the running entry by prefix so it actually
        // resolves instead of lingering as Running forever.
        let matched_running = if let Some(execution) =
            self.tool_executions.iter_mut().rev().find(|e| {
                e.status == ToolStatus::Running
                    && (e.tool_name == tool_name
                        || tool_name.starts_with(&format!("{} ", e.tool_name))
                        || tool_name.contains(&e.tool_name))
            }) {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
            true
        } else {
            false
        };

        if !matched_running && self.tool_row_completed_like(tool_name) {
            return;
        }

        self.current_tool = None;
        self.set_phase(GenPhase::Summarizing);

        // Header on its own line: the chat widget renders `[header]` as a
        // `⚙/✗ header` row with the summary body beneath it. Trim blank lines
        // around the body so the row never opens with an empty line.
        // When the row streamed live, fill it in place — no reorder, no
        // duplicate block at task end. Robust: header may have been updated
        // via ToolProgress, so the live placeholder check may miss — fall
        // back to matching by header prefix before appending.
        let body = Self::trim_blank_lines(result);
        let is_error = body.trim_start().starts_with("Error:");
        let header = match duration_ms {
            Some(ms) => format!("{tool_name} · {}", kod_core::engine::format_duration_ms(ms)),
            None => tool_name.to_string(),
        };
        let content = format!("[{header}]\n{body}");
        let msg_id = if let Some(i) = self.live_tool_msg() {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else if let Some(i) = self.messages.iter().rposition(|m| {
            m.role == MessageRole::Tool && m.content.starts_with(&format!("[{}]", tool_name))
        }) {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else if let Some(i) = self.messages.iter().rposition(|m| {
            // Header was rewritten by ToolProgress — match by tool base name
            let header = m.content.lines().next().unwrap_or("").trim();
            let header = header
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(header);
            m.role == MessageRole::Tool && tool_name.contains(header)
                || header.contains(tool_name.split_whitespace().next().unwrap_or(""))
        }) {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else {
            let msg = Message {
                id: MessageId::new(),
                role: MessageRole::Tool,
                content,
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            };
            let id = msg.id.clone();
            self.add_message(msg);
            id
        };
        // Errors must be unmistakable: auto-expand so the full message is
        // visible and never hidden behind the 12-line preview.
        if is_error {
            self.expanded_tools.insert(msg_id);
        }
    }

    /// True when a finished (non-placeholder) tool row already exists for
    /// `tool_name`. Used to recognize the task-end fallback arriving after
    /// a live done-marker completed the row — symmetric with the
    /// `Running`-entry match above, plus the reverse direction because the
    /// live row's header carries the `· duration` stamp.
    fn tool_row_completed_like(&self, tool_name: &str) -> bool {
        self.messages.iter().any(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let mut parts = m.content.splitn(2, '\n');
            let first = parts.next().unwrap_or("").trim();
            let header = first
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(first);
            let body = parts.next().unwrap_or("").trim();
            if body.is_empty() || body == Self::LIVE_TOOL_BODY_PLACEHOLDER {
                return false;
            }
            header == tool_name
                || header.starts_with(&format!("{tool_name} "))
                || tool_name.starts_with(&format!("{header} "))
                || header.contains(tool_name)
                || tool_name.contains(header)
        })
    }

    pub fn fail_tool_execution(&mut self, tool_name: &str, error: &str) {
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.status == ToolStatus::Running
                && (e.tool_name == tool_name
                    || tool_name.starts_with(&format!("{} ", e.tool_name))
                    || tool_name.contains(&e.tool_name))
        }) {
            execution.status = ToolStatus::Failed;
            execution.result = Some(error.to_string());
        }

        self.current_tool = None;

        let body = format!("Error: {}", error.trim());
        let content = format!("[{}]\n{}", tool_name, body);
        let msg_id = if let Some(i) = self.live_tool_msg() {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else {
            let msg = Message {
                id: MessageId::new(),
                role: MessageRole::Tool,
                content,
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            };
            let id = msg.id.clone();
            self.add_message(msg);
            id
        };
        // Always expand errors — same rationale as complete_tool_execution.
        self.expanded_tools.insert(msg_id);
    }

    // Streaming response
    pub fn current_response(&self) -> &str {
        &self.current_response
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn start_response_stream(&mut self) {
        self.is_streaming = true;
        self.current_response.clear();
    }

    pub fn add_response_chunk(&mut self, chunk: &str) {
        if self.is_streaming {
            self.current_response.push_str(chunk);
            if self.phase == GenPhase::Connecting {
                self.set_phase(GenPhase::Generating);
            }
        }
    }

    pub fn complete_response(&mut self) {
        if self.is_streaming {
            let response = Self::trim_blank_lines(&self.current_response);
            if !response.is_empty() {
                self.add_message(Message {
                    id: MessageId::new(),
                    role: MessageRole::Assistant,
                    content: response,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                });
                self.stream_flushed_bubble = true;
            }

            self.is_streaming = false;
            self.current_response.clear();
        }
    }

    /// Mark a prompt as sent: shows the thinking indicator until the
    /// response (or error) arrives. Also arms the streaming accumulator
    /// so `ResponseChunk` events are kept.
    pub fn begin_generation(&mut self) {
        self.generating = true;
        self.spinner_started = Some(Instant::now());
        self.stream_flushed_bubble = false;
        self.last_error = None;
        // New turn: no real usage seen yet, so the char estimate
        // contributes until (or unless) the provider reports a total.
        self.turn_has_real_usage = false;
        self.set_phase(GenPhase::Connecting);
        self.start_response_stream();
    }

    /// Flush whatever the stream accumulated so far as its own assistant
    /// bubble, keeping the stream open. Called when a tool call starts or
    /// lands so text stays interleaved with tool rows in event order
    /// instead of merging into one giant trailing bubble.
    pub fn flush_streamed_text(&mut self) {
        let text = Self::trim_blank_lines(&self.current_response);
        self.current_response.clear();
        if !text.is_empty() {
            self.note_usage(text.len());
            self.push_assistant_message(&text);
            self.stream_flushed_bubble = true;
            self.maybe_compact();
        }
    }

    /// Finish a generation. Pushes whatever the stream accumulated, or
    /// `fallback` (the engine's full reply text) when nothing streamed.
    /// When per-round text was already flushed into bubbles (tool runs),
    /// the accumulator is empty *because it was shown* — pushing the
    /// fallback then would reprint the whole reply as one giant trailing
    /// bubble, so it is skipped. Always prefer streamed chunks; fallback
    /// only when nothing was ever flushed.
    pub fn finish_response(&mut self, fallback: &str) {
        let text = if !self.current_response.is_empty() {
            Self::trim_blank_lines(&self.current_response.clone())
        } else if self.stream_flushed_bubble {
            String::new()
        } else {
            Self::trim_blank_lines(fallback)
        };
        if !text.is_empty() {
            self.note_usage(text.len());
            self.push_assistant_message(&text);
            self.stream_flushed_bubble = true;
            self.maybe_compact();
        }

        // Force the chat to show the bottom of the new message — previously
        // push_assistant_message only pinned when already at bottom, so an
        // even-slight scroll-up left the final line clipped and G/End could
        // miscompute the viewport. Chat apps always land at bottom.
        self.scroll_to_bottom();

        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count = 0;
    }

    /// Clear the live "running …" line and settle any entries still
    /// marked Running (error / cancel paths). Without this the indicator
    /// sticks around and later messages land around it unpredictably.
    fn settle_running_tools(&mut self, status: ToolStatus, note: &str) {
        for execution in self.tool_executions.iter_mut() {
            if execution.status == ToolStatus::Running {
                execution.status = status;
                execution.result = Some(note.to_string());
            }
        }
        // Stamp live rows too so cancel/error never leaves a stale
        // "running…" row behind.
        for m in self.messages.iter_mut() {
            if m.role != MessageRole::Tool {
                continue;
            }
            let mut parts = m.content.splitn(2, '\n');
            parts.next();
            if parts.next().unwrap_or("").trim() == Self::LIVE_TOOL_BODY_PLACEHOLDER {
                let header = m.content.lines().next().unwrap_or("").to_string();
                m.content = format!("{header}\n{note}");
            }
        }
        self.current_tool = None;
    }

    /// Record a generation failure: clears the thinking indicator and
    /// surfaces an actionable error as a system message.
    pub fn fail_generation(&mut self, error: &str) {
        // Same rationale as cancel_generation: settle the flag so the
        // next turn does not inherit "real usage seen" from this one.
        self.turn_has_real_usage = false;
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count += 1;
        self.settle_running_tools(ToolStatus::Failed, error);
        self.last_error = Some(error.to_string());
        self.push_system_message(&Self::friendly_error(error, self.fail_count));
    }

    /// Turn a raw provider/transport error into something the user can act
    /// on. The original text is always kept — advice is appended.
    ///
    /// Ordering matters: "model not found" is often reported by OpenAI-
    /// compatible servers as a 404, and context-length errors sometimes
    /// arrive wrapped in a `400` or `422`. Match the most specific
    /// phrasing first.
    pub fn friendly_error(error: &str, fail_count: usize) -> String {
        let lower = error.to_lowercase();
        let advice = if lower.contains("model not found")
            || lower.contains("model `")
            || lower.contains("unknown model")
            || lower.contains("no such model")
            || lower.contains("model does not exist")
        {
            // The most common first-run mistake: config or `/model <name>`
            // names a model the server has never pulled. The fix is one
            // command, so name it.
            " The model named in the config or `/model` is not on the server.              Pull it first (e.g. `ollama pull codellama:13b`), or run `/model <name>`              with a model the server already has."
        } else if lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("too many tokens")
            || lower.contains("maximum context")
            || lower.contains("exceeds the maximum")
        {
            " The prompt exceeded the model's context window. Run `/compact` to trim              the session, or start a fresh chat with `/clear`."
        } else if (lower.contains("json") && lower.contains("parse"))
            || lower.contains("invalid tool")
            || lower.contains("malformed function")
            || lower.contains("tool_call")
        {
            " The model returned a tool call that could not be parsed. Retrying              usually helps — if it persists, the model may not support tool calling              at all (try a larger or newer model, or a codellama/qwen2.5-coder build)."
        } else if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("failed to connect")
            || lower.contains("connection closed")
        {
            " Could not reach the model server — is it running? For Ollama: `ollama serve`, then check `base_url` in the kod config."
        } else if lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("api key")
        {
            " Looks like an auth problem — check `api_key` in the kod config."
        } else if lower.contains("404") {
            " Endpoint not found — check `base_url` ends with `/v1` for OpenAI-compatible servers."
        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure) and matches the previous
            // behavior of returning an empty advice string for both.
            ""
        };
        let mut out = format!("Error: {error}");
        out.push_str(advice);
        if fail_count >= 2 {
            out.push_str(
                " (offline mode: generation keeps failing — fix the server, then `/retry`)",
            );
        }
        out
    }

    /// Cancel a running prompt (Esc / Ctrl+C / `/cancel`). Keeps whatever
    /// text already streamed as a partial assistant reply, then announces
    /// the cancel so the stop is visible — not a silent stall.
    pub fn cancel_generation(&mut self) {
        // The turn is over; a subsequent prompt must see a fresh
        // turn state even if begin_generation is not called before
        // next note_prompt. begin_generation resets this again.
        self.turn_has_real_usage = false;
        let partial = Self::trim_blank_lines(&self.current_response);
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.set_phase(GenPhase::Idle);
        self.settle_running_tools(ToolStatus::Failed, "cancelled");
        if !partial.is_empty() {
            self.note_usage(partial.len());
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::Assistant,
                content: format!("{partial}\n(cancelled — partial answer)"),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            });
            self.stream_flushed_bubble = true;
        } else {
            self.push_system_message("Cancelled.");
        }
    }

    /// Set the persistent goal (`/goal <text>`). While set, every prompt
    /// runs the goal loop: the agent keeps working turn by turn until it
    /// writes GOAL MET. Shown in the header.
    pub fn set_goal(&mut self, goal: &str) {
        let goal = goal.trim();
        if goal.is_empty() {
            return;
        }
        self.goal = Some(goal.to_string());
    }

    pub fn clear_goal(&mut self) {
        self.goal = None;
    }

    pub fn goal(&self) -> Option<&str> {
        self.goal.as_deref()
    }

    pub fn is_generating(&self) -> bool {
        self.generating
    }

    pub fn set_generating(&mut self, generating: bool) {
        self.generating = generating;
    }

    /// Advance the thinking spinner one frame (called on every tick).
    /// Kept for tick-driven callers; the live frame is time-based (see
    /// `spinner`) so chunk floods can't freeze it.
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Current spinner frame. While generating, the frame derives from
    /// wall-clock time (≈10 fps), so every render advances it even when
    /// `ResponseChunk` traffic starves `Tick` events.
    pub fn spinner(&self) -> &'static str {
        if self.generating
            && let Some(started) = self.spinner_started
        {
            let step = started.elapsed().as_millis() / 100;
            return SPINNER_FRAMES[(step as usize) % SPINNER_FRAMES.len()];
        }
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    pub fn set_model_name(&mut self, name: &str) {
        self.model_name = name.to_string();
    }

    // Session context accounting + auto-compact

    /// Rough token estimate for a piece of text (1 token ≈ 4 chars). Approx until provider `usage` lands.
    fn estimate_tokens(chars: usize) -> usize {
        chars / 4
    }

    /// Record tokens for an outgoing prompt (approx).
    pub fn note_prompt(&mut self, prompt: &str) {
        self.note_usage(prompt.len());
        self.maybe_compact();
    }

    /// Record real token usage from the provider.
    ///
    /// Real usage is authoritative: the server reports exactly how many
    /// tokens went through the model on the last call. Replace the
    /// running char-based estimate rather than taking the max — the
    /// estimate accumulates across turns and never resets, so `max`
    /// made the meter monotonically climb across a session even though
    /// the model's context stays bounded by `render_history` + the
    /// provider's own window. That mis-report also drove auto-compact
    /// into firing far earlier than the real threshold.
    ///
    /// (The previous implementation used `max`, contradicting its own
    /// doc comment which said "replaces".)
    pub fn note_real_usage(&mut self, total_tokens: usize) {
        if total_tokens > 0 {
            self.context_tokens = total_tokens;
            // Once real usage has arrived, the char estimate must stop
            // contributing for the rest of the turn. The provider's
            // total already covers prompt + completion, so any
            // subsequent `note_usage` from `finish_response` /
            // `flush_streamed_text` would double-count the reply.
            self.turn_has_real_usage = true;
        }
        self.maybe_compact();
    }

    /// Record real usage from a TokenUsage-like triple (prompt/completion/total).
    pub fn note_usage_tokens(
        &mut self,
        prompt_tokens: usize,
        completion_tokens: usize,
        total_tokens: usize,
    ) {
        let total = if total_tokens > 0 {
            total_tokens
        } else {
            prompt_tokens + completion_tokens
        };
        self.note_real_usage(total);
    }

    fn note_usage(&mut self, chars: usize) {
        // Suppressed once real usage for this turn has been recorded.
        // See `turn_has_real_usage` for the double-count this avoids.
        if self.turn_has_real_usage {
            return;
        }
        self.context_tokens = self
            .context_tokens
            .saturating_add(Self::estimate_tokens(chars));
    }

    pub fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    pub fn context_limit(&self) -> usize {
        self.context_limit
    }

    pub fn set_context_limit(&mut self, limit: usize) {
        self.context_limit = limit.max(1_000);
    }

    /// `≈ ctx 12.4k/128k · 10%` — human-readable session position.
    /// `context_tokens` is an approximation (chars/4) until the provider
    /// returns real `usage` — prefix with ≈ so the header never implies precision.
    pub fn context_label(&self) -> String {
        let pct = self.context_tokens * 100 / self.context_limit.max(1);
        format!(
            "≈ ctx {}/{} · {}%",
            Self::format_k(self.context_tokens),
            Self::format_k(self.context_limit),
            pct.min(999)
        )
    }

    fn format_k(n: usize) -> String {
        if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    /// Drop oldest messages once past 4/5 of the window, keeping the most
    /// recent 20. Announces itself so the user sees where they stand.
    fn maybe_compact(&mut self) {
        let threshold = self.context_limit * COMPACT_AT_FRACTION_NUM / COMPACT_AT_FRACTION_DEN;
        if self.context_tokens < threshold || self.messages.len() <= 21 {
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        // Re-baseline: remaining history ≈ 3/5 of the window.
        self.context_tokens = self.context_limit * 3 / 5;
        let note = format!(
            "Auto-compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        );
        // Route through add_message so the notice gets a monotonic
        // sequence. Pushing directly with `sequence: 0` made the chat
        // widget (which sorts by sequence) render the notice at the
        // very top of the transcript, above the messages it had just
        // compacted — the exact opposite of where it belongs.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: note,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Manual `/compact`: same as auto but on demand.
    ///
    /// Both branches already went through `push_system_message` (which
    /// calls `add_message`), so the notice has always had a correct
    /// sequence — kept here as the counterpart to `maybe_compact`, and
    /// to make it obvious that both paths must go through `add_message`.
    pub fn compact_now(&mut self) {
        if self.messages.len() <= 21 {
            self.push_system_message(&format!("Nothing to compact · {}", self.context_label()));
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        self.context_tokens = self.context_limit * 3 / 5;
        self.push_system_message(&format!(
            "Compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        ));
    }

    // Skills visible to the session

    pub fn set_loaded_skills(&mut self, names: Vec<String>) {
        self.loaded_skills = names;
    }

    pub fn loaded_skills(&self) -> &[String] {
        &self.loaded_skills
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

    // Slash-command completion (first token only — once there's a space,
    // the user is typing arguments, where path/model completion takes over).

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

    // Filesystem path completion for argument tokens:
    // `@src/ma`, `read ./Cargo`, `open ~/Doc`, `diff a/b|c`,
    // `read "my docs/rep`. Quotes and spaces inside quotes are honored.
    // Triggers on the last whitespace-separated token when it looks like
    // a path (leading `@`, or contains `/`, or starts with `.` / `~`).

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

    // ---- Swarm runs ----

    /// Prepare for a new swarm run: clears the live-agent map so a
    /// previous run's rows are not appended to.
    pub fn begin_swarm(&mut self) {
        self.swarm_agents.clear();
    }

    /// Announce the decompose results as a system line.
    pub fn swarm_decomposed(&mut self, subtasks: &[(String, String)]) {
        let mut s = format!("Swarm: {} subtasks\n", subtasks.len());
        for (i, (name, desc)) in subtasks.iter().enumerate() {
            let d = desc.lines().next().unwrap_or(desc);
            s.push_str(&format!("  {}. {} — {}\n", i + 1, name, d));
        }
        self.push_system_message(s.trim_end());
    }

    /// Create a chat row for a starting agent.
    pub fn swarm_agent_started(
        &mut self,
        id: kod_types::AgentId,
        name: &str,
        subtask: &str,
    ) {
        let header = format!(
            "{name} — {}",
            subtask.lines().next().unwrap_or(subtask)
        );
        let msg_id = MessageId::new();
        self.add_message(Message {
            id: msg_id.clone(),
            role: MessageRole::Agent(id.clone()),
            content: header,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        self.swarm_agents.insert(
            id,
            SwarmAgentView {
                message_id: msg_id,
                finished: false,
            },
        );
    }

    /// Append a text chunk to a live agent's row.
    pub fn swarm_agent_chunk(&mut self, id: &kod_types::AgentId, text: &str) {
        let Some(view) = self.swarm_agents.get(id) else {
            return;
        };
        if view.finished {
            return;
        }
        let msg_id = view.message_id.clone();
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            msg.content.push_str(text);
        }
    }

    /// Replace the live row's trailing buffer with the agent's final
    /// result. The header stays.
    pub fn swarm_agent_finished(&mut self, id: &kod_types::AgentId, result: &str) {
        let Some(view) = self.swarm_agents.get_mut(id) else {
            return;
        };
        let msg_id = view.message_id.clone();
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            let header = msg
                .content
                .split_once('\n')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| msg.content.clone());
            let body = Self::trim_blank_lines(result);
            msg.content = if body.is_empty() {
                header
            } else {
                format!("{header}\n{body}")
            };
        }
        view.finished = true;
    }

    /// Mark a live row as failed and replace its buffer with the error.
    pub fn swarm_agent_failed(&mut self, id: &kod_types::AgentId, error: &str) {
        let Some(view) = self.swarm_agents.get_mut(id) else {
            return;
        };
        let msg_id = view.message_id.clone();
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            let header = msg
                .content
                .split_once('\n')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| msg.content.clone());
            msg.content = format!("{header}\n(failed: {})", error.trim());
        }
        view.finished = true;
    }

    /// Finish the swarm: push the merged answer as an assistant row.
    pub fn swarm_complete(&mut self, merged: &str) {
        if !merged.trim().is_empty() {
            self.push_assistant_message(merged);
        }
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count = 0;
        self.scroll_to_bottom();
    }

    // Persistence: prompt history + session restore.
    //
    // Prompt history lives in `~/.kod/tui_history.json` (cap 500) and is
    // loaded at startup / appended on submit — power users keep history
    // across restarts. Sessions (chat messages) persist to
    // `~/.kod/tui_session.json` so a restart resumes where you left off.
    // `KOD_TUI_STATE_DIR` overrides the directory (tests use it).

    fn state_dir() -> Option<std::path::PathBuf> {
        if let Ok(dir) = std::env::var("KOD_TUI_STATE_DIR") {
            return Some(std::path::PathBuf::from(dir));
        }
        dirs::home_dir().map(|h| h.join(".kod"))
    }

    pub fn history_path() -> Option<std::path::PathBuf> {
        Self::state_dir().map(|d| d.join("tui_history.json"))
    }

    pub fn session_path() -> Option<std::path::PathBuf> {
        Self::state_dir().map(|d| d.join("tui_session.json"))
    }

    /// Load persisted prompt history (startup). Best-effort: missing or
    /// corrupt files just mean a fresh history.
    pub fn load_persistent_history(&mut self) {
        let Some(path) = Self::history_path() else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Ok(entries) = serde_json::from_str::<Vec<String>>(&raw) {
            for e in entries.into_iter().take(500) {
                if !e.trim().is_empty() && !self.input_history.contains(&e) {
                    self.input_history.push(e);
                }
            }
            if self.input_history.len() > 500 {
                let drop = self.input_history.len() - 500;
                self.input_history.drain(..drop);
            }
        }
    }

    /// Append one entry to the history file (called after submit).
    ///
    /// Two TUI sessions running concurrently (one in each of two
    /// worktrees, say) read and write the same
    /// `~/.kod/tui_history.json`. The previous unlocked
    /// read-modify-write could interleave (A reads, B writes, A
    /// writes) and silently discard everything B appended.
    ///
    /// Rather than take an advisory lock — the `fs4` crate's module
    /// path depends on the feature set and version, and this crate
    /// does not otherwise need it — write to a sibling temp file and
    /// rename over the target. `rename` is atomic on POSIX and on
    /// Windows (via ReplaceFile semantics under the std
    /// implementation), so no reader ever sees a partially written
    /// file. The worst case is one session's last append losing to
    /// the other's — the same last-writer-wins as before, without
    /// the risk of a truncated read.
    ///
    /// Best-effort throughout: history is a convenience, never a
    /// correctness requirement, and a failed save must not surface as
    /// an error.
    pub fn persist_history_entry(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() {
            return;
        }
        // Keep the in-memory list in sync when dispatch bypassed submit.
        if self.input_history.last().map(|s| s.as_str()) != Some(entry) {
            self.input_history.push(entry.to_string());
        }
        let Some(path) = Self::history_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(parent);

        // Merge with whatever is on disk.
        let mut entries: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        if entries.last().map(|s| s.as_str()) != Some(entry) {
            entries.push(entry.to_string());
        }
        if entries.len() > 500 {
            let drop = entries.len() - 500;
            entries.drain(..drop);
        }

        // Write to a unique sibling, then rename over the target.
        // The pid+suffix keeps two sessions from colliding on the
        // temp file itself.
        let tmp = parent.join(format!(
            "tui_history.json.tmp.{}.{}",
            std::process::id(),
            // Nanoseconds since the epoch, cheap unique-ish suffix
            // without pulling in a random-number crate.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let serialized = match serde_json::to_string(&entries) {
            Ok(s) => s,
            Err(_) => return,
        };
        if std::fs::write(&tmp, serialized.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            // Rename failed (cross-device? permissions?). Clean up the
            // temp so we do not accumulate orphans, and give up.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Save the current chat for session restore (called on quit /
    /// after each assistant reply — cheap enough at chat scale).
    ///
    /// Writes to a sibling temp file then renames over the target.
    /// `std::fs::write` truncates the destination before writing, so a
    /// crash between the truncate and the last byte left a zero-byte
    /// or half-written session file — which `load_session` then
    /// discards, silently losing the transcript the user was trying
    /// to save. The rename is atomic on POSIX and Windows, so a
    /// reader either sees the complete previous file or the complete
    /// new one.
    ///
    /// Also removes the second-writer hazard the same way
    /// `persist_history_entry` does: two TUI processes shutting down
    /// concurrently can each serialize a session, but the loser's
    /// rename is the only observable outcome. No interleaved partial
    /// file.
    pub fn save_session(&self) {
        let Some(path) = Self::session_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let keep = self.messages.len().saturating_sub(200);
        let snapshot = &self.messages[keep..];
        let _ = std::fs::create_dir_all(parent);

        let Ok(raw) = serde_json::to_string(snapshot) else {
            return;
        };

        // Unique temp per process + nanosecond clock. Two processes
        // writing at once get different temps and the rename lets the
        // last one win; neither leaves a partial file behind.
        let tmp = parent.join(format!(
            "tui_session.json.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        if std::fs::write(&tmp, raw.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            // Rename failed (cross-device temp, permissions on the
            // target directory). Clean up the temp; the previous
            // session file remains untouched on disk.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Restore the saved chat. Returns the restored message count.
    pub fn load_session(&mut self) -> usize {
        let Some(path) = Self::session_path() else {
            return 0;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return 0;
        };
        let Ok(mut msgs) = serde_json::from_str::<Vec<Message>>(&raw) else {
            return 0;
        };
        let n = msgs.len();
        // Restore monotonic sequence after a restart: `next_seq` must
        // be past the highest stored sequence, otherwise new messages
        // would sort before restored ones.
        //
        // Pre-sequence session files (all `sequence == 0`) are
        // backfilled in file order below, which changes `next_seq`.
        // Files written by the current code (any non-zero sequence)
        // use the max-sequence path.
        let max_seq = msgs.iter().map(|m| m.sequence).max().unwrap_or(0);
        self.next_seq = max_seq + msgs.len() as u64 + 1;

        if msgs.iter().all(|m| m.sequence == 0) && !msgs.is_empty() {
            // Legacy file: assign in file order so the chat widget's
            // sequence sort is a no-op on this file. `next_seq` is set
            // to `len()` so the next new message lands after all of
            // them. (The previous implementation had this branch
            // preceded by an empty `for` loop that did nothing — a
            // leftover from an earlier design that never ran.)
            for (i, m) in msgs.iter_mut().enumerate() {
                m.sequence = i as u64;
            }
            self.next_seq = msgs.len() as u64;
        }

                self.messages = msgs;
        self.scroll_to_bottom();
        n
    }

    // Quit handling
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn set_should_quit(&mut self, quit: bool) {
        self.should_quit = quit;
    }

    /// Open the type-ahead search bar with an empty query.
    ///
    /// While the bar is open, `handle_key` routes printable characters
    /// and Backspace into the query (see `is_editing_search`), Enter
    /// commits (leaving the query navigable with `n`/`N`), and Escape
    /// clears the search outright. This is the state `/search` with no
    /// argument opens.
    pub fn begin_search(&mut self) {
        self.search_query = Some(String::new());
        self.search_index = 0;
        self.search_editing = true;
    }

    /// True while the search bar is open and the user is typing into
    /// it. Distinct from `is_searching()` (which requires a non-empty
    /// query): during editing there are no matches yet and no
    /// match-position to report, but the bar is on screen and every
    /// keystroke is the user's query.
    pub fn is_editing_search(&self) -> bool {
        self.search_editing && self.search_query.is_some()
    }

    /// Leave the editing state without dropping the query: the search
    /// remains active (`is_searching()` is unchanged), but typing
    /// stops going into the query, and `n`/`N` navigate matches.
    /// Enter calls this.
    pub fn commit_search(&mut self) {
        self.search_editing = false;
    }

    /// Type into the active search (appended to the query).
    pub fn search_type(&mut self, c: char) {
        if let Some(q) = &mut self.search_query {
            q.push(c);
        }
        let query = self.search_query_text().to_string();
        let n = self.set_search(&query);
        if n > 0 {
            self.jump_to_search_match(self.search_index);
        }
    }

    /// Backspace the search query.
    pub fn search_backspace(&mut self) {
        if let Some(q) = &mut self.search_query {
            q.pop();
        }
        let query = self.search_query_text().to_string();
        let n = self.set_search(&query);
        if n > 0 {
            self.jump_to_search_match(self.search_index);
        }
    }

    /// Copy the last assistant reply to the system clipboard (the `y` key).
    pub fn copy_last_to_clipboard(&self) -> bool {
        let Some(text) = self.last_assistant_text() else {
            return false;
        };
        crate::clipboard::write_clipboard(text)
    }

    /// Set the list of models the provider offers (for `/model` completion).
    pub fn set_available_models(&mut self, models: Vec<String>) {
        self.available_models = models;
    }

    /// The models the provider last reported. Empty when no list has
    /// been fetched yet (engine not initialized, list_models failed).
    /// Callers that want to validate a `/model <name>` request must
    /// treat empty as "unknown — do not warn" rather than "no models
    /// exist", because an empty list is also what a cold provider
    /// returns before the first list_models call.
    pub fn available_models(&self) -> &[String] {
        &self.available_models
    }
}

/// State of the chat search, as reported by [`KodApp::search_status`].
///
/// The previous API, `search_position() -> (usize, usize)`, collapsed
/// three distinct states into two indistinguishable pairs:
///
///   * no query at all             -> (0, 0)
///   * query, no matches           -> (0, 0)
///   * query, match 1 of 1         -> (1, 1)
///
/// A caller reading `(0, 0)` could not tell whether to say "no
/// search" or "no matches", and the status widget guessed "no
/// matches" — so a session that had never been searched could show
/// "no matches" the moment a search state existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchStatus {
    /// No search query at all. Status bar shows the idle hint.
    Inactive,
    /// Search bar open, query empty, user has not typed a character
    /// yet. Distinct from `NoMatches`, which requires a non-empty
    /// query that found nothing.
    Editing,
    /// Non-empty query, zero matches found.
    NoMatches,
    /// Non-empty query with matches. `position` is 1-based and always
    /// `<= total` (the modulo happens in `search_status`, not at the
    /// call site).
    At { position: usize, total: usize },
}

/// Which completion popup list is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    None,
    Slash,
    Model,
    Path,
}

/// Subsequence match: every char of `query` appears in `name` in order.
/// Powers the forgiving (`/md` → `/model`) completion tier.
fn fuzzy_match(name: &str, query: &str) -> bool {
    let mut q = query.chars();
    let mut cur = q.next();
    if cur.is_none() {
        return true;
    }
    for c in name.chars() {
        if Some(c) == cur {
            cur = q.next();
            if cur.is_none() {
                return true;
            }
        }
    }
    false
}

impl Default for KodApp {
    fn default() -> Self {
        Self::new()
    }
}

// Widget-support accessors: thin views over state so the ui/ modules stay
// rendering-only (theme, phase, search, confirm, clipboard helpers).
impl KodApp {
    /// Help overlay visibility. `AppMode::Help` (legacy `h`/`?` path) also
    /// counts so both spellings open the same overlay.
    pub fn show_help(&self) -> bool {
        self.show_help || matches!(self.mode, AppMode::Help)
    }

    /// Toggle the help overlay. Closing also leaves `AppMode::Help`.
    pub fn toggle_help(&mut self) {
        if self.show_help() {
            self.show_help = false;
            if matches!(self.mode, AppMode::Help) {
                self.mode = AppMode::Normal;
            }
        } else {
            self.show_help = true;
        }
    }

    /// Current spinner glyph (alias the widgets use).
    pub fn spinner_frame(&self) -> &'static str {
        self.spinner()
    }

    /// `12s` since the generation started (empty when idle).
    pub fn elapsed_label(&self) -> String {
        match self.spinner_started {
            Some(s) => format!("{}s", s.elapsed().as_secs()),
            None => String::new(),
        }
    }

    /// Consecutive generation failures (offline badge + backoff hints).
    pub fn consecutive_failures(&self) -> usize {
        self.fail_count
    }

    /// Reset the failure streak (after `/retry` or a success).
    pub fn reset_failures(&mut self) {
        self.fail_count = 0;
    }

    /// Raw last error for the status bar (chat holds the friendly version).
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Active theme name (`dark` / `light`).
    pub fn theme_name(&self) -> &str {
        &self.theme.name
    }

    /// Context fill ratio 0.0–1.0+ (rough estimate, see context_label).
    pub fn context_usage(&self) -> f64 {
        self.context_tokens as f64 / self.context_limit.max(1) as f64
    }

    /// Model name for the header (never blank).
    pub fn model_label(&self) -> &str {
        if self.model_name.is_empty() {
            "no model"
        } else {
            &self.model_name
        }
    }

    /// Total input tokens the provider has reported this session.
    pub fn session_input_tokens(&self) -> usize {
        self.session_input_tokens
    }

    /// Total output tokens the provider has reported this session.
    pub fn session_output_tokens(&self) -> usize {
        self.session_output_tokens
    }

    /// Total tokens moved through the model this session.
    pub fn session_total_tokens(&self) -> usize {
        self.session_input_tokens
            .saturating_add(self.session_output_tokens)
    }

    /// Wall-clock time since the session started.
    pub fn elapsed_session(&self) -> std::time::Duration {
        self.session_started_at.elapsed()
    }

    /// One-line accounting label for the header: `↑1.2k ↓340 · 5m30s`.
    /// The arrow convention is input/output; the trailing figure is
    /// wall-clock elapsed since the first prompt.
    pub fn accounting_label(&self) -> String {
        let secs = self.elapsed_session().as_secs();
        let time = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
        };
        format!(
            "↑{} ↓{} · {}",
            Self::format_k(self.session_input_tokens),
            Self::format_k(self.session_output_tokens),
            time,
        )
    }

    /// Record session-wide token usage from a per-call breakdown.
    /// Separate from `note_real_usage` (window snapshot for the context
    /// meter): this counter only grows.
    pub fn note_session_usage(
        &mut self,
        prompt_tokens: usize,
        completion_tokens: usize,
    ) {
        self.session_input_tokens = self
            .session_input_tokens
            .saturating_add(prompt_tokens);
        self.session_output_tokens = self
            .session_output_tokens
            .saturating_add(completion_tokens);
    }

    /// Context-aware hint line for the status bar.
    pub fn hint_line(&self) -> String {
        if matches!(self.input_mode, InputMode::Insert) {
            "Enter send · Ctrl+J newline · Up history · Tab complete · Esc done".to_string()
        } else if self.generating {
            "Esc cancel · /steer redirect · ? help".to_string()
        } else {
            // `/ search` used to sit here, but the search key is `f`
            // (SearchPrefix); `/` opens the command slot. Name the
            // actual key so the hint is not a small lie.
            "i type · / command · j/k scroll · t tools · f search · ? help · q quit".to_string()
        }
    }

    /// Newest running tool for the status bar: (total, done, label).
    /// Tool rows don't carry step counts, so total/done are 0/0 unless a
    /// progress display was pushed via `update_tool_status`.
    pub fn active_tool(&self) -> Option<(usize, usize, String)> {
        let name = self.current_tool.clone()?;
        Some((0, 0, name))
    }

    /// True while a search query is active (status bar + highlighting).
    pub fn is_searching(&self) -> bool {
        self.search_query.as_deref().is_some_and(|q| !q.is_empty())
    }


    /// Active search text (empty when no search).
    pub fn search_query_text(&self) -> &str {
        self.search_query.as_deref().unwrap_or("")
    }

    /// What to say about the current search in the status bar.
    ///
    /// The old `search_position() -> (usize, usize)` collapsed three
    /// distinct states into two indistinguishable pairs:
    ///
    ///   * search not active            -> (0, 0)
    ///   * search active, no matches    -> (0, 0)
    ///   * search active, match 1 of 1  -> (1, 1)
    ///
    /// A caller reading `(0, 0)` could not tell whether to say "no
    /// search" or "no matches" — and the widget that renders the search
    /// status guessed "no matches", so a session that had never been
    /// searched still showed "no matches" the moment a search state
    /// existed. The enum below names the states; the widget matches on
    /// it and the label is derived here, once.
    pub fn search_status(&self) -> SearchStatus {
        match self.search_query.as_deref() {
            None => SearchStatus::Inactive,
            Some("") => SearchStatus::Editing,
            Some(_) => {
                let total = self.search_matches().len();
                if total == 0 {
                    SearchStatus::NoMatches
                } else {
                    SearchStatus::At {
                        position: (self.search_index % total) + 1,
                        total,
                    }
                }
            }
        }
    }

    /// The message id currently targeted by the active search, if
    /// any. Returns `None` when no search query is set, when the
    /// query is empty (editing), or when it found no matches.
    ///
    /// Used by the chat widget to scroll the targeted message into
    /// view. The underlying `search_index` is private so the widget
    /// cannot reach it directly; this exposes exactly what the widget
    /// needs (the id of the match to center) without exposing the
    /// index arithmetic.
    pub fn search_target_message_id(&self) -> Option<&kod_types::MessageId> {
        let matches = self.search_matches();
        if matches.is_empty() {
            return None;
        }
        let pos = self.search_index % matches.len();
        let msg_idx = *matches.get(pos)?;
        self.messages().get(msg_idx).map(|m| &m.id)
    }

    /// Display string for the status bar. Built here so the widget does
    /// not re-implement the match on `SearchStatus` and drift.
    pub fn search_status_label(&self) -> String {
        match self.search_status() {
            SearchStatus::Inactive => String::new(),
            SearchStatus::Editing => format!(" /{} — typing… (Esc exits)", self.search_query_text()),
            SearchStatus::NoMatches => format!(
                " /{} — no matches (Esc exits)",
                self.search_query_text()
            ),
            SearchStatus::At { position, total } => format!(
                " /{} — {}/{} (n next · N prev · Esc exits)",
                self.search_query_text(),
                position,
                total
            ),
        }
    }

    /// Last assistant reply text (for `y` / `/copy`).
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
            .map(|m| m.content.as_str())
    }

    /// Expand the newest collapsed tool message in place (key `o`).
    /// Returns false when there is nothing expandable.
    pub fn expand_newest_tool(&mut self) -> bool {
        let id = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Tool && !self.expanded_tools.contains(&m.id))
            .map(|m| m.id.clone());
        match id {
            Some(id) => {
                self.expanded_tools.insert(id);
                self.scroll_to_bottom();
                true
            }
            None => false,
        }
    }

    /// Move the cursor one word left (Ctrl+Left).
    pub fn move_cursor_word_left(&mut self) {
        if self.cursor_position == 0 {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut pos = self.cursor_position;
        while pos > 0 && bytes[pos - 1] == b' ' {
            pos -= 1;
        }
        while pos > 0 && bytes[pos - 1] != b' ' && bytes[pos - 1] != b'\n' {
            pos -= 1;
        }
        while pos > 0 && !self.input.is_char_boundary(pos) {
            pos -= 1;
        }
        self.cursor_position = pos;
    }

    /// Move the cursor one word right (Ctrl+Right).
    pub fn move_cursor_word_right(&mut self) {
        let len = self.input.len();
        if self.cursor_position >= len {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut pos = self.cursor_position;
        while pos < len && bytes[pos] != b' ' && bytes[pos] != b'\n' {
            pos += 1;
        }
        while pos < len && bytes[pos] == b' ' {
            pos += 1;
        }
        while pos < len && !self.input.is_char_boundary(pos) {
            pos += 1;
        }
        self.cursor_position = pos;
    }

    /// Delete from the cursor to the end of its line (Ctrl+K).
    pub fn cut_to_end(&mut self) {
        let end = self.input[self.cursor_position..]
            .find('\n')
            .map(|i| self.cursor_position + i)
            .unwrap_or(self.input.len());
        self.input.drain(self.cursor_position..end);
    }

    /// Delete the whole input line(s) (Ctrl+U clears to start; this clears
    /// everything — used when the box holds a failed one-liner).
    pub fn clear_line(&mut self) {
        self.clear_input();
    }
}

#[cfg(test)]
mod tests {

    /// Serializes tests that mutate KOD_TUI_STATE_DIR (a process-wide
    /// environment variable). Rust runs unit tests in parallel by
    /// default, and two tests racing to set the same env var would
    /// step on each other's state directories.
    fn session_state_dir_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
    use super::*;

    #[test]
    fn test_context_limit_overrides_default() {
        let mut app = KodApp::new();
        assert_eq!(app.context_limit(), DEFAULT_CONTEXT_LIMIT);
        // init_engine applies config.llm.context_window so the meter and
        // the compaction threshold use the model's real window, not 128k.
        app.set_context_limit(8192);
        assert_eq!(app.context_limit(), 8192);
        assert!(
            app.context_label().contains("8.2k"),
            "got: {}",
            app.context_label()
        );
    }

    #[test]
    fn test_app_lifecycle() {
        let mut app = KodApp::new();

        app.set_input_mode(InputMode::Insert);
        app.add_char('t');
        app.add_char('e');
        app.add_char('s');
        app.add_char('t');

        assert_eq!(app.input(), "test");

        app.submit_input();
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.input(), "");
    }

    #[test]
    fn test_agent_management() {
        let mut app = KodApp::new();
        app.add_agent("agent1", vec!["coding".to_string()]);
        assert_eq!(app.agents().len(), 1);
        assert!(app.get_agent("agent1").is_some());
        app.update_agent_status("agent1", "working");
        assert_eq!(app.get_agent("agent1").unwrap().status, "working");
    }

    #[test]
    fn test_message_display() {
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "hello".to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Real usage is authoritative and must REPLACE the char-based
    /// estimate, not be maxed against it. Regression: the running
    /// estimate accumulates across turns (`note_prompt` for each user
    /// input, `note_usage` for each streamed response) and never
    /// resets, so `max(estimate, real)` grew unboundedly. After a
    /// dozen turns the meter reported a context far larger than the
    /// model ever saw, and auto-compact fired early because the
    /// threshold is checked against the same number.
    #[test]
    fn test_real_usage_replaces_estimate() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        // Turn 1: estimate from a 4000-char prompt then a 2000-char
        // streamed response. context_tokens ≈ (4000 + 2000) / 4 = 1500.
        app.note_prompt(&"a".repeat(4000));
        let est_after_turn_1 = app.context_tokens();
        assert!(est_after_turn_1 > 0);

        // The provider then reports real usage of 800. It must replace
        // the estimate, not be maxed against it.
        app.note_real_usage(800);
        assert_eq!(
            app.context_tokens(),
            800,
            "real usage should replace the estimate; got {}",
            app.context_tokens()
        );

        // Turn 2: estimate grows again, but the next real usage resets.
        app.note_prompt(&"b".repeat(4000));
        app.note_real_usage(900);
        assert_eq!(app.context_tokens(), 900);

        // Sanity: turning off real usage (0 tokens) leaves the previous
        // value alone rather than zeroing it — providers that omit
        // usage on a round shouldn't blank the meter.
        app.note_real_usage(0);
        assert_eq!(app.context_tokens(), 900);
    }

    /// Real usage replaces the estimate even when the estimate is
    /// *smaller*. Prior behavior took the max, so a low real usage
    /// after a high estimate never brought the meter down.
    #[test]
    fn test_real_usage_can_shrink_the_meter() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        app.note_prompt(&"x".repeat(40_000)); // ≈ 10_000 tokens
        let big_estimate = app.context_tokens();
        assert!(big_estimate >= 10_000);

        app.note_real_usage(500);
        assert_eq!(app.context_tokens(), 500);
    }

    /// Auto-compact must append its notice at the end of the transcript,
    /// not sort it to the top. The chat widget sorts by `sequence`, and
    /// the old `maybe_compact` pushed a message with `sequence: 0`,
    /// which sorted before every user message in the session.
    #[test]
    fn test_auto_compact_notice_sorts_after_compacted_messages() {
        let mut app = KodApp::new();
        // Force the auto-compact threshold with a tiny context window.
        app.set_context_limit(1_000);

        // Add enough messages to cross the 4/5 threshold AND exceed the
        // 21-message guard that protects against compacting an empty or
        // short session.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        // Push token usage past 4/5 of 1000 = 800.
        app.note_real_usage(900);

        // The notice must be the last message by sequence.
        let last = app.messages().last().expect("at least one message");
        let last_seq = last.sequence;
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Auto-compacted"),
            "last message should be the compaction notice, got: {}",
            last.content
        );

        // And no other message has a sequence greater than it (trivially
        // true) or equal-and-later-positioned at the same sequence.
        for m in app.messages().iter().take(app.messages().len() - 1) {
            assert!(
                m.sequence < last_seq,
                "a message sorts after the compaction notice: seq {} vs {}",
                m.sequence,
                last_seq
            );
        }
    }

    /// The manual `/compact` path must also leave its notice at the end.
    #[test]
    fn test_manual_compact_notice_sorts_after() {
        let mut app = KodApp::new();
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.compact_now();

        let last = app.messages().last().expect("at least one message");
        assert_eq!(last.role, MessageRole::System);
        assert!(
            last.content.contains("Compacted"),
            "last message should be the manual compaction notice, got: {}",
            last.content
        );
    }

    /// A pre-sequence session file (all sequences == 0) must backfill
    /// in file order, so the chat widget's sequence sort is a no-op on
    /// the restored transcript. The dead loop the previous
    /// implementation carried never did anything; the branch that
    /// actually works is covered here.
    #[test]
    fn test_load_session_backfills_legacy_sequences() {
        use crate::app::{KodApp, Message};
        use kod_types::{MessageId, MessageMetadata, MessageRole};

        let tmp = std::env::temp_dir().join(format!(
            "kod-tui-legacy-seq-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _guard = session_state_dir_lock();
        // SAFETY: serialized via the shared mutex.
        unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };

        // Write a session file where every message has sequence 0 —
        // the shape produced before sequences existed. Write the
        // messages through their serde shape (the on-disk form), not a
        // hand-built json! whose field names would silently drift.
        let path = std::env::temp_dir().join(format!(
            "kod-tui-legacy-seq-test-{}/tui_session.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            serde_json::to_string(&[
                Message {
                    id: MessageId::new(),
                    role: MessageRole::User,
                    content: "first".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
                Message {
                    id: MessageId::new(),
                    role: MessageRole::Assistant,
                    content: "second".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
                Message {
                    id: MessageId::new(),
                    role: MessageRole::User,
                    content: "third".into(),
                    timestamp: chrono::Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                },
            ])
            .unwrap(),
        )
        .unwrap();

        let mut app = KodApp::new();
        let n = app.load_session();
        assert_eq!(n, 3);

        // Sequences are 0, 1, 2 in file order.
        let seqs: Vec<u64> = app.messages().iter().map(|m| m.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);

        // A new message pushed afterwards sorts after all restored ones.
        app.push_system_message("new");
        let last = app.messages().last().unwrap();
        assert!(
            last.sequence > 2,
            "new message should sort after backfilled ones: seq {}",
            last.sequence
        );

        unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Real token usage replaces the char estimate, and subsequent
    /// estimate calls in the same turn must not stack on top of it.
    /// Regression: TokenUsage arrives before ResponseComplete in the
    /// event queue; finish_response then called note_usage, adding
    /// the reply's chars/4 to a total that already included the
    /// completion — every turn over-reported by roughly the reply
    /// length.
    #[test]
    fn test_real_usage_suppresses_further_estimates() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        // New turn: estimate-only until real usage arrives.
        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000 estimate
        assert!(app.context_tokens() >= 1_000);
        let after_prompt = app.context_tokens();

        // Provider reports the real total (already includes the
        // completion). Replaces the estimate.
        app.note_real_usage(1_500);
        assert_eq!(app.context_tokens(), 1_500);

        // finish_response adds the reply's chars/4 via note_usage;
        // gated by the flag, it must be a no-op.
        let reply = "y".repeat(2_000); // would be +500 if applied
        app.finish_response(&reply);
        assert_eq!(
            app.context_tokens(),
            1_500,
            "post-usage estimate must not stack (was {} before reply, {} after)",
            after_prompt,
            app.context_tokens(),
        );
    }

    /// A turn where the provider reports no usage must fall back to
    /// the char estimate, unchanged from before.
    #[test]
    fn test_estimate_still_works_without_real_usage() {
        let mut app = KodApp::new();
        app.set_context_limit(100_000);

        app.begin_generation();
        app.note_prompt(&"x".repeat(4_000)); // ≈ 1_000
        let after_prompt = app.context_tokens();
        assert!(after_prompt >= 1_000);

        app.finish_response(&"y".repeat(4_000)); // ≈ +1_000
        assert!(
            app.context_tokens() > after_prompt,
            "estimate must accumulate when no real usage arrives"
        );
    }

    /// The type-ahead search states: begin_search opens the bar with
    /// an empty query, typing appends, Enter commits (query stays,
    /// editing stops), Escape/clear_search drops everything.
    #[test]
    fn test_type_ahead_search_state_transitions() {
        let mut app = KodApp::new();
        assert!(!app.is_editing_search());
        assert!(!app.is_searching());

        // Open the bar.
        app.begin_search();
        assert!(app.is_editing_search(), "begin_search must enter editing");
        assert_eq!(app.search_query_text(), "");
        assert_eq!(app.search_status(), SearchStatus::Editing);

        // Typing appends.
        app.search_type('h');
        app.search_type('i');
        assert_eq!(app.search_query_text(), "hi");
        assert!(app.is_editing_search());

        // Add a matching message so the search finds something.
        app.push_system_message("a hit for hi");
        // Re-run the search against the new message set.
        let _ = app.set_search("hi");

        // Commit: query stays, editing ends, is_searching() is true.
        app.commit_search();
        assert!(!app.is_editing_search());
        assert!(app.is_searching());
        assert_eq!(app.search_query_text(), "hi");

        // Clear: everything goes away.
        app.clear_search();
        assert!(!app.is_editing_search());
        assert!(!app.is_searching());
        assert_eq!(app.search_query_text(), "");
        assert_eq!(app.search_status(), SearchStatus::Inactive);
    }

    /// `search_status` must distinguish the states the old
    /// `search_position` tuple collapsed together.
    #[test]
    fn test_search_status_distinguishes_states() {
        let mut app = KodApp::new();
        assert_eq!(app.search_status(), SearchStatus::Inactive);
        assert_eq!(app.search_status_label(), "");

        app.begin_search();
        assert_eq!(app.search_status(), SearchStatus::Editing);
        assert!(
            app.search_status_label().contains("typing"),
            "editing label should say typing: {}",
            app.search_status_label()
        );

        app.search_query = Some("nothing-will-match-this".to_string());
        assert_eq!(app.search_status(), SearchStatus::NoMatches);
        assert!(
            app.search_status_label().contains("no matches"),
            "label should say no matches: {}",
            app.search_status_label()
        );

        app.push_system_message("haystack one");
        app.push_system_message("haystack two");
        app.search_query = Some("haystack".to_string());
        match app.search_status() {
            SearchStatus::At { position, total } => {
                assert_eq!(total, 2);
                assert_eq!(position, 1);
            }
            other => panic!("expected At, got {other:?}"),
        }
        let label = app.search_status_label();
        assert!(label.contains("1/2"), "label should say 1/2: {label}");
    }

    /// `search_next` / `search_prev` wrap the position and the position
    /// reported by `search_status` stays in 1..=total.
    #[test]
    fn test_search_position_wraps() {
        let mut app = KodApp::new();
        for i in 0..3 {
            app.push_system_message(&format!("needle {i}"));
        }
        let n = app.set_search("needle");
        assert_eq!(n, 3);

        assert!(matches!(
            app.search_status(),
            SearchStatus::At { position: 1, total: 3 }
        ));
        app.search_next();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At { position: 2, total: 3 }
        ));
        app.search_next();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At { position: 3, total: 3 }
        ));
        app.search_next();
        assert!(
            matches!(app.search_status(), SearchStatus::At { position: 1, total: 3 }),
            "next should wrap: {:?}",
            app.search_status()
        );
        app.search_prev();
        assert!(matches!(
            app.search_status(),
            SearchStatus::At { position: 3, total: 3 }
        ));
    }

    /// A failed save must leave the previous session file intact.
    /// Regression: the old `std::fs::write` truncated the target
    /// before writing, so a mid-write failure (disk full, crash,
    /// permission flip) left a zero-byte file — which load_session
    /// then discarded, losing the transcript.
    ///
    /// The temp+rename implementation writes the temp first and only
    /// renames on success, so the target is either the old complete
    /// file or the new complete file, never a partial one. This test
    /// simulates the failure by pointing KOD_TUI_STATE_DIR at a path
    /// whose parent cannot be created, and asserts the previously
    /// saved file still loads.
    #[test]
    fn test_save_session_does_not_corrupt_previous_file() {
        use crate::app::{KodApp, Message};
        use kod_types::{MessageId, MessageMetadata, MessageRole};

        let tmp = std::env::temp_dir().join(format!(
            "kod-tui-save-safety-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let _guard = session_state_dir_lock();
        // SAFETY: serialized via the shared mutex.
        unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };

        // Write a valid session.
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "the only message".into(),
            timestamp: chrono::Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        app.save_session();

        // A fresh load must see the message.
        let mut restored = KodApp::new();
        assert_eq!(restored.load_session(), 1);
        assert_eq!(restored.messages()[0].content, "the only message");

        // Overwrite with an empty session — this is the path a user
        // takes after `/clear`. The previous file must be replaced
        // whole; a reader that arrives mid-write must not see a
        // half-empty file.
        let empty = KodApp::new();
        empty.save_session();

        let mut after = KodApp::new();
        // The file now serializes an empty array; load_session returns 0.
        assert_eq!(after.load_session(), 0);
        assert!(after.messages().is_empty());

        unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `/clear` resets the context accounting. Regression: the
    /// visible messages and the engine transcript were reset, but
    /// `context_tokens` and `compacted_messages` kept accumulating,
    /// so the header meter overstated the current context and the
    /// auto-compact threshold was compared against a number that
    /// included discarded messages.
    #[test]
    fn test_clear_resets_context_accounting() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);

        // Build up a believable pre-clear state: some messages and
        // some token usage.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.note_real_usage(3_000);
        // Also trigger a manual compact to set compacted_messages.
        app.compact_now();
        assert!(app.context_tokens() > 0);
        assert!(app.messages().len() < 30, "compact should have dropped some");

        // Sanity: pre-clear state is not the fresh state.
        let pre_tokens = app.context_tokens();

        app.clear_messages();

        assert!(app.messages().is_empty(), "display should be empty");
        assert_eq!(
            app.context_tokens(),
            0,
            "context accounting must reset (was {pre_tokens})"
        );
        // The label must report 0% — the meter the header draws reads
        // from the same counter.
        assert!(
            app.context_label().contains("0%"),
            "context label should read 0%: {}",
            app.context_label()
        );
    }

    /// `/undo` restores the cleared messages but must NOT resurrect
    /// the stale context count — the engine's transcript was cleared
    /// by the `/clear` handler, so the model really does have zero
    /// context at that point. Undo is a display operation only.
    #[test]
    fn test_undo_does_not_restore_stale_context() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);
        app.push_system_message("hello");
        app.note_real_usage(2_500);
        assert_eq!(app.context_tokens(), 2_500);

        app.clear_messages();
        assert_eq!(app.context_tokens(), 0);

        let restored = app.undo_clear();
        assert!(restored, "undo should succeed");
        assert_eq!(app.messages().len(), 1);
        // Context stays at the post-clear value, not the pre-clear one.
        assert_eq!(
            app.context_tokens(),
            0,
            "undo must not resurrect a stale context count"
        );
    }

    #[test]
    fn test_scroll_to_bottom() {
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "test".to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        assert!(app.is_scrolled_to_bottom());
    }

    /// The swarm state machine: decompose adds a system line; agent
    /// start creates a chat row; chunks append to that row; completion
    /// replaces the trailing buffer; merged answer lands as an
    /// assistant message.
    #[test]
    fn test_swarm_state_machine() {
        let mut app = KodApp::new();
        app.begin_swarm();
        app.begin_generation();

        // Decompose → system line.
        app.swarm_decomposed(&[
            ("schema".to_string(), "write the SQL schema".to_string()),
            ("api".to_string(), "implement the handler".to_string()),
        ]);
        let sys = app.messages().last().unwrap();
        assert_eq!(sys.role, MessageRole::System);
        assert!(sys.content.contains("2 subtasks"));
        assert!(sys.content.contains("schema"));

        // Agent start → a chat row keyed by the agent id.
        let id_a = kod_types::AgentId::new();
        let id_b = kod_types::AgentId::new();
        app.swarm_agent_started(id_a.clone(), "agent-1", "write the SQL schema");
        app.swarm_agent_started(id_b.clone(), "agent-2", "implement the handler");
        assert_eq!(app.messages().len(), 3, "two system/agent rows after decompose + starts");

        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .expect("agent-1 row");
        assert!(row_a.content.contains("agent-1"));

        // Chunks append to the right row.
        app.swarm_agent_chunk(&id_a, "\nworking");
        app.swarm_agent_chunk(&id_a, " on it");
        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .unwrap();
        assert!(row_a.content.contains("working on it"), "got: {}", row_a.content);

        // Chunks after finish are ignored.
        app.swarm_agent_finished(&id_a, "DONE: schema written");
        app.swarm_agent_chunk(&id_a, "late noise");
        let row_a = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_a))
            .unwrap();
        assert!(!row_a.content.contains("late noise"));
        assert!(row_a.content.contains("DONE: schema written"));
        assert!(row_a.content.contains("agent-1"), "header preserved: {}", row_a.content);

        // Failure replaces the buffer.
        app.swarm_agent_failed(&id_b, "boom");
        let row_b = app
            .messages()
            .iter()
            .find(|m| matches!(&m.role, MessageRole::Agent(a) if a == &id_b))
            .unwrap();
        assert!(row_b.content.contains("failed"));
        assert!(row_b.content.contains("boom"));

        // Swarm complete lands the merged answer as an assistant row and
        // ends the generation state.
        assert!(app.is_generating());
        app.swarm_complete("MERGED: schema + handler + tests");
        assert!(!app.is_generating());
        let merged = app.messages().last().unwrap();
        assert_eq!(merged.role, MessageRole::Assistant);
        assert!(merged.content.contains("MERGED"));
    }

    /// `begin_swarm` clears a previous run's live-agent views, so a
    /// second run does not append to the first run's finished rows.
    #[test]
    fn test_begin_swarm_resets_live_views() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "agent-1", "first run");
        // Finish it so the view is marked done, but keep the map entry.
        app.swarm_agent_finished(&id, "done");
        assert!(app.swarm_agents.contains_key(&id));

        app.begin_swarm();
        assert!(app.swarm_agents.is_empty(), "begin_swarm clears the live map");
    }

}
