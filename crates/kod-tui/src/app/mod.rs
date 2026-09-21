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

/// Streaming response lifecycle (extracted from this file).
mod streaming;

/// Input buffer, cursor, and history ring.
mod input;

/// Tool-execution rows: start / update / complete / fail.
mod tools;

/// Message-list state.
mod messages;

/// In-transcript search state.
mod search;

/// Swarm agent state.
mod swarm;

/// Tab-completion state (slash, model, path).
mod completion;

/// Command palette state.
mod palette;

/// Partial-hunk approval state.
mod hunks;

/// Prompt history + session persistence.
mod persistence;

/// Context-window accounting and auto-compact.
mod context;

/// Theme, phase, dialogs, header/status accessors, export helpers.
mod ui_state;

/// Current on-disk session schema version. Bump when a field
/// changes meaning, not merely when a field is added — the loader
/// tolerates unknown fields.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// The current on-disk shape of `~/.kod/tui_session.json` (Tier 3.2).
///
/// Pre-3.2 files are a bare JSON array of `Message`. Those are
/// loaded via the legacy fallback in `load_session`; any file written
/// from this build carries `schema_version` and `messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub schema_version: u32,
    pub messages: Vec<Message>,
}

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

/// A live swarm agent's chat row and the data the panel needs to
/// render a status line. The message id is stable for the agent's
/// lifetime so chunks append in place rather than spawning a new row
/// per token; the remaining fields are populated by the Swarm*
/// events the engine emits.
#[derive(Debug, Clone)]
pub struct SwarmAgentView {
    pub message_id: MessageId,
    pub finished: bool,
    /// Display name (`agent-1-design-schema`).
    pub name: String,
    /// Short subtask description, first line only.
    pub subtask: String,
    /// Model name, when known.
    pub model: Option<String>,
    /// Worktree path, when a per-agent worktree was created.
    pub worktree: Option<std::path::PathBuf>,
    /// Worktree branch name.
    pub branch: Option<String>,
    /// Number of tool-call chunks seen so far.
    pub tool_count: usize,
    /// Set to `Some(reason)` when the agent failed.
    pub failure: Option<String>,
    /// Set to `Some(note)` when the agent was retried; cleared on the
    /// next successful finish.
    pub retry_note: Option<String>,
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

/// One entry in the command palette (Ctrl+K). Derived from
/// `SLASH_COMMANDS` plus keybindings, so a new command or key appears
/// automatically (Tier UX).
#[derive(Debug, Clone)]
pub struct CommandPaletteEntry {
    pub label: String,
    pub hint: String,
    /// The value inserted into the input when the entry is accepted.
    /// For a slash command this is the command name; for a keybinding
    /// it is the key's description for the help card.
    pub insert: String,
}

/// Command palette state. `None` on the app means "closed"; the
/// palette is a single-use overlay, not a mode.
#[derive(Debug, Clone, Default)]
pub struct CommandPaletteState {
    pub query: String,
    pub selected: usize,
}

impl CommandPaletteState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter the palette's entry list by the current query. The
    /// comparison is case-insensitive substring on the label; an
    /// empty query returns the full list.
    pub fn filtered(&self, all: &[CommandPaletteEntry]) -> Vec<CommandPaletteEntry> {
        if self.query.is_empty() {
            return all.to_vec();
        }
        let q = self.query.to_lowercase();
        all.iter()
            .filter(|e| e.label.to_lowercase().contains(&q) || e.hint.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }
}

/// Build the palette's entry list. Slash commands come from
/// `SLASH_COMMANDS`; the remaining entries surface the cheat-sheet
/// keys so a user can find them by typing.
pub fn build_palette_entries() -> Vec<CommandPaletteEntry> {
    let mut entries: Vec<CommandPaletteEntry> = SLASH_COMMANDS
        .iter()
        .map(|c| CommandPaletteEntry {
            label: c.name.to_string(),
            hint: c.hint.to_string(),
            insert: c.name.to_string(),
        })
        .collect();
    // A small set of key-only actions that a user might search for.
    for (label, hint, key) in [
        ("ctrl+k", "command palette", "Ctrl+K"),
        ("ctrl+e", "edit last message", "Ctrl+E"),
        ("ctrl+u", "delete to line start", "Ctrl+U"),
        ("ctrl+w", "delete previous word", "Ctrl+W"),
        ("ctrl+j", "insert newline in input", "Ctrl+J"),
        ("? / h", "help overlay", "?"),
        ("t", "toggle tool output", "t"),
        ("g / G", "scroll top / bottom", "g/G"),
        ("q", "quit", "q"),
        ("esc", "cancel running prompt", "Esc"),
    ] {
        entries.push(CommandPaletteEntry {
            label: label.to_string(),
            hint: hint.to_string(),
            insert: key.to_string(),
        });
    }
    entries
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
    SlashCommand {
        name: "/doctor",
        hint: "print a diagnostics report (same as `kod doctor`)",
    },
    SlashCommand {
        name: "/init",
        hint: "onboarding info: config path, model profiles, next steps",
    },
    SlashCommand {
        name: "/regenerate",
        hint: "regenerate the last assistant reply",
    },
    SlashCommand {
        name: "/delete",
        hint: "remove the last user+assistant exchange",
    },
    SlashCommand {
        name: "/export",
        hint: "export session as markdown: /export [path]",
    },
    SlashCommand {
        name: "/export-html",
        hint: "export session as a self-contained HTML file: /export-html [path]",
    },
    SlashCommand {
        name: "/memory",
        hint: "long-term memory: /memory [search <q> | delete <id> | clear]",
    },
    SlashCommand {
        name: "/remember",
        hint: "store a durable fact in long-term memory: /remember <text>",
    },
    SlashCommand {
        name: "/policy",
        hint: "tool policy: /policy [show | forget <n>]",
    },
    SlashCommand {
        name: "/map",
        hint: "print the repository map (top-level symbols per file)",
    },
    SlashCommand {
        name: "/context",
        hint: "visualize context window usage and session totals",
    },
    SlashCommand {
        name: "/last-prompt",
        hint: "shortcut for /debug last-prompt",
    },
    SlashCommand {
        name: "/diff",
        hint: "show the most recent file diff (from checkpoints)",
    },
    SlashCommand {
        name: "/attach",
        hint: "attach a file to the next prompt: /attach <path>",
    },
    SlashCommand {
        name: "/refine",
        hint: "refine the last assistant reply: /refine <instruction>",
    },
    SlashCommand {
        name: "/raw",
        hint: "print the last assistant reply raw (no decoration)",
    },
    SlashCommand {
        name: "/save",
        hint: "save session to a file: /save <path>",
    },
    SlashCommand {
        name: "/load",
        hint: "load session from a JSON file: /load <path>",
    },
    SlashCommand {
        name: "/branch",
        hint: "drop a branch-point marker: /branch [label]",
    },
    SlashCommand {
        name: "/system",
        hint: "override the system prompt: /system <text> | /system clear",
    },
    SlashCommand {
        name: "/grep",
        hint: "regex search the chat history: /grep <regex>",
    },
    SlashCommand {
        name: "/summarize",
        hint: "LLM-summarize the session so far",
    },
    SlashCommand {
        name: "/whoami",
        hint: "session summary: model, skills, context, paths",
    },
    SlashCommand {
        name: "/clearall",
        hint: "clear chat + memory + checkpoints (asks for confirmation)",
    },
    SlashCommand {
        name: "/stats",
        hint: "per-session statistics: roles, tools, tokens, elapsed",
    },
    SlashCommand {
        name: "/git-status",
        hint: "git status --porcelain=v2 in the current directory",
    },
    SlashCommand {
        name: "/reset",
        hint: "reset transient state: input, search, expansions, attachments",
    },
    SlashCommand {
        name: "/fork",
        hint: "save the current chat as a restorable fork: /fork [label]",
    },
    SlashCommand {
        name: "/check",
        hint: "run the project compiler/linter (Cargo, tsc, ruff, go vet)",
    },
    SlashCommand {
        name: "/trace",
        hint: "structured turn traces: /trace [last | list]",
    },
    SlashCommand {
        name: "/trust",
        hint: "show or clear the round's taint: /trust [show | clear]",
    },
    SlashCommand {
        name: "/blackboard",
        hint: "swarm blackboard: /blackboard [show | clear]",
    },
    SlashCommand {
        name: "/learned",
        hint: "list or clear session-scoped learned approvals: /learned [clear]",
    },
    SlashCommand {
        name: "/decisions",
        hint: "durable decisions this session: /decisions [drop <id> | clear]",
    },
    SlashCommand {
        name: "/plan",
        hint: "show plan: /plan [next | skip | note <text> | clear]",
    },
    SlashCommand {
        name: "/limits",
        hint: "per-tool quotas: /limits [show | reset]",
    },
    SlashCommand {
        name: "/budget",
        hint: "session cost and limits: /budget | /budget raise <usd> | /budget reset",
    },
    SlashCommand {
        name: "/jev",
        hint: "TypeSafe AI integration: /jev [status | stats | cache clear | test]",
    },
    SlashCommand {
        name: "/log",
        hint: "show recent session log entries: /log [N]",
    },
    SlashCommand {
        name: "/pin",
        hint: "pin a message so it survives history compaction: /pin <n>",
    },
    SlashCommand {
        name: "/unpin",
        hint: "remove a pin: /unpin <n>",
    },
    SlashCommand {
        name: "/handoff",
        hint: "write a handoff document and start a fresh session with it as context",
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
    /// Clear chat, long-term memory, AND checkpoints for this project.
    /// The most destructive action; the caller must request it
    /// explicitly.
    ClearAll,
}

/// Spinner frames for the "thinking" indicator
pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Main application state
pub struct KodApp {
    mode: AppMode,
    /// Command palette overlay (Ctrl+K). `None` when closed.
    palette: Option<CommandPaletteState>,
    /// Approval-edit modal (Tier 2.3). `None` when not editing.
    pending_edit: Option<PendingEdit>,
    /// Partial-hunk approval mode (Tier 2.3). `None` when not
    /// selecting hunks.
    pending_hunk_selection: Option<PendingHunkSelection>,
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
    /// Time the first non-empty streamed text chunk of the current
    /// turn arrived. `None` until that chunk lands, and reset by
    /// `begin_generation`. The latency from `spinner_started` to
    /// this instant is the turn's time-to-first-token.
    first_chunk_at: Option<Instant>,
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
    /// Agent ids in the order they started. Powers the `@N`
    /// focus syntax in `dispatch_prompt`: `@2 hello` steers the
    /// second agent that began work in the current run. Cleared
    /// by `begin_swarm` alongside `swarm_agents`.
    swarm_agent_order: Vec<kod_types::AgentId>,
    /// Markdown render cache (design D6.5). The chat widget
    /// re-renders every visible message every frame; without a
    /// cache that means re-parsing the markdown on each keystroke,
    /// a cost that grows with the transcript length. The cache is
    /// keyed by (content, width, theme) and bounded FIFO.
    render_cache: std::sync::Arc<crate::markdown::RenderCache>,
    /// The pending approval batch the TUI is showing a dialog for.
    /// `None` when no dialog is up. Populated from an
    /// approval-batch marker chunk; cleared when every item has been
    /// decided (or Esc denies the remainder).
    pending_batch: Option<PendingApprovalBatch>,
    /// The ask_user question the TUI is currently prompting for.
    /// Populated from a question-marker chunk; cleared on answer.
    pending_question: Option<PendingQuestion>,
    /// The buffer the user is typing into while a question is up.
    question_input: String,
    /// Mid-session system prompt override. Prepended to every prompt
    /// when set. Cleared with `/system clear`.
    session_system_prompt: Option<String>,
    /// When true (the default), `maybe_compact` fires when the context
    /// crosses 4/5 of the window. When false, the user controls
    /// compaction entirely with `/compact`.
    autocompact_enabled: bool,
    /// When true, a long turn rings the terminal bell. Toggle with
    /// `/notify on|off`. Default true.
    notify_bell_enabled: bool,
    /// Whether the effective network access is on. Set by
    /// `TuiLoop::init_engine` from the engine's setting; drives the
    /// header `net:on` badge (D3-C5).
    network_access_enabled: bool,
    /// The effective sandbox label for the header, e.g. `bwrap`,
    /// `landlock`, `sandbox-exec`, `require-missing`, or `off`.
    /// Set by `TuiLoop::init_engine` from
    /// `KodEngine::sandbox_status`. The empty string means "no
    /// engine yet" — the header renders no badge until this is
    /// populated, so a session that never touched a shell command
    /// does not lie about its sandbox state.
    sandbox_label: String,

    /// Wall clock of the last non-empty streamed chunk. Used to
    /// measure the streaming duration for the `tok/s` figure in
    /// the status bar. Reset by `begin_generation` and cleared
    /// by `finish_response` / `fail_generation`.
    last_chunk_at: Option<Instant>,

    /// Characters streamed in the current turn (before the
    /// 4-chars-per-token approximation). The rate figure is
    /// `(streamed_chars / 4) / seconds-since-first-chunk`.
    streamed_chars_this_turn: usize,
    /// Files attached to the next prompt with `/attach`. Prepended as
    /// `<file path="...">` blocks to the outgoing message. Cleared
    /// after the prompt is dispatched.
    attached_files: Vec<std::path::PathBuf>,
    /// Wall-clock instant the session started.
    session_started_at: Instant,
    /// Accumulated input tokens the provider has reported this session.
    /// Distinct from `context_tokens` (which is a window snapshot and
    /// shrinks under compaction); this counter only grows.
    session_input_tokens: usize,
    /// Accumulated USD cost of every reported provider call this
    /// session. Advances only when the endpoint carries a
    /// `[pricing]` block; stays at 0.0 otherwise, in which case the
    /// header does not display a `$` figure.
    session_cost_usd: f64,
    /// Accumulated output tokens the provider has reported this session.
    session_output_tokens: usize,
    /// When the current generation started. Used to decide whether a
    /// completion is worth a terminal bell — a 2-second turn is not.
    turn_started_at: Option<Instant>,
    should_quit: bool,
    next_seq: u64,
}

/// Default context window assumed when the provider reports none.
pub const DEFAULT_CONTEXT_LIMIT: usize = 128_000;
/// Fraction of the window that triggers auto-compact.
pub const COMPACT_AT_FRACTION_NUM: usize = 4;
pub const COMPACT_AT_FRACTION_DEN: usize = 5;

/// Tier 2.3 — an in-progress approval-argument edit. The user has
/// pressed `e` in the approval dialog; the input box edits the call's
/// JSON arguments instead of the prompt.
#[derive(Debug, Clone)]
pub struct PendingEdit {
    /// Approval id whose call is being edited.
    pub approval_id: u64,
    /// Original arguments, kept so Esc can restore.
    pub original: serde_json::Value,
    /// Current text in the input box.
    pub buffer: String,
}

/// Tier 2.3 — an in-progress partial-hunk approval. The user has
/// pressed `h` in the approval dialog for a `patch_file` call; the
/// dialog now shows each hunk as a toggle. Enter commits a filtered
/// patch and sends `ApproveWith`.
#[derive(Debug, Clone)]
pub struct PendingHunkSelection {
    /// Approval id whose patch is being edited.
    pub approval_id: u64,
    /// The call's arguments, with `patch` still the original.
    pub original_arguments: serde_json::Value,
    /// Everything in the patch before the first `@@` header.
    pub header: String,
    /// The hunks, each starting at `@@`.
    pub hunks: Vec<String>,
    /// One flag per hunk.
    pub selected: Vec<bool>,
    /// Index of the highlighted hunk.
    pub cursor: usize,
}

impl PendingHunkSelection {
    /// The currently highlighted index, clamped to the hunk range.
    pub fn current(&self) -> Option<usize> {
        if self.hunks.is_empty() {
            None
        } else {
            Some(self.cursor.min(self.hunks.len() - 1))
        }
    }

    /// Toggle the current hunk's selection.
    pub fn toggle_current(&mut self) {
        if let Some(i) = self.current() {
            self.selected[i] = !self.selected[i];
        }
    }

    pub fn advance(&mut self) {
        if !self.hunks.is_empty() {
            self.cursor = (self.cursor + 1) % self.hunks.len();
        }
    }

    pub fn retreat(&mut self) {
        if !self.hunks.is_empty() {
            self.cursor = (self.cursor + self.hunks.len() - 1) % self.hunks.len();
        }
    }

    /// Build the filtered patch string from the selected hunks.
    pub fn build_patch(&self) -> String {
        let mut out = self.header.clone();
        for (i, h) in self.hunks.iter().enumerate() {
            if self.selected.get(i).copied().unwrap_or(false) {
                out.push_str(h);
            }
        }
        out
    }

    /// Number of selected hunks.
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|b| **b).count()
    }
}

/// Split a unified diff into `(header, hunks)`. A hunk begins at a
/// line starting with `@@`; everything before the first `@@` is the
/// header. An empty diff yields `("", [])`.
pub fn split_hunks(diff: &str) -> (String, Vec<String>) {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in diff.split_inclusive('\n') {
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(line.to_string());
        } else if let Some(h) = current.as_mut() {
            h.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    if let Some(h) = current {
        hunks.push(h);
    }
    (header, hunks)
}

/// An approval request currently waiting for a yes/no answer in the
/// TUI. The `id` matches the engine's request id; the decision is
/// sent back via `KodEngine::respond_to_approval`.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: u64,
    pub tool_name: String,
    pub summary: String,
    pub diff: Option<String>,

    /// The call's arguments (Tier 2.3). Carried so the "learn an
    /// allow" action can hash the exact call rather than its
    /// displayed summary.
    pub arguments: serde_json::Value,
}

/// A batch of pending approvals — the shape the engine emits per
/// round. The TUI shows the current item and walks the batch with
/// y/n/a (which decide and advance) and ↑/↓ (which navigate without
/// deciding).
#[derive(Debug, Clone)]
pub struct PendingApprovalBatch {
    /// The engine's batch id, for logging.
    pub batch_id: u64,
    pub items: Vec<PendingApproval>,
    /// Index of the item currently shown. Advanced past
    /// `items.len()` when every item has been decided; the engine
    /// treats a batch as closed once the caller stops sending
    /// decisions.
    pub current: usize,
}

impl PendingApprovalBatch {
    pub fn current_item(&self) -> Option<&PendingApproval> {
        self.items.get(self.current)
    }
    /// Move to the next item. Returns `true` when the move succeeded
    /// (there was a next item), `false` when the batch was already on
    /// its last item.
    pub fn advance(&mut self) -> bool {
        if self.current + 1 < self.items.len() {
            self.current += 1;
            true
        } else {
            // Move past the end so `current_item()` returns None and
            // the caller knows the batch is exhausted.
            self.current = self.items.len();
            false
        }
    }
    /// Move to the previous item. Returns `true` on success.
    pub fn retreat(&mut self) -> bool {
        if self.current > 0 {
            self.current -= 1;
            true
        } else {
            false
        }
    }
}

/// A question currently waiting for a text answer in the TUI.
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub id: u64,
    pub question: String,
    pub placeholder: Option<String>,
}

impl KodApp {
    pub fn new() -> Self {
        Self {
            mode: AppMode::Normal,
            palette: None,
            pending_edit: None,
            pending_hunk_selection: None,
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
            first_chunk_at: None,
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
            swarm_agent_order: Vec::new(),
            render_cache: std::sync::Arc::new(crate::markdown::RenderCache::new()),
            pending_batch: None,
            pending_question: None,
            question_input: String::new(),
            session_system_prompt: None,
            autocompact_enabled: true,
            notify_bell_enabled: true,
            network_access_enabled: false,
            sandbox_label: String::new(),
            last_chunk_at: None,
            streamed_chars_this_turn: 0,
            attached_files: Vec::new(),
            session_started_at: Instant::now(),
            session_input_tokens: 0,
            session_output_tokens: 0,
            session_cost_usd: 0.0,
            turn_started_at: None,
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

    // --- Command palette (Tier UX) --------------------------------------

    // --- Partial-hunk approval (Tier 2.3) -------------------------------

    // --- Approval edit modal (Tier 2.3) --------------------------------

    // Input handling
    // Message management
    // Pending confirmations (destructive actions)

    // Chat search

    // Tool output expand / hide

    // Generation phases, retry state, themes

    // NOTE: the previous `goal_progress() -> Option<(usize, usize)>`
    // returned a fixed `Some((0, 1))` whenever a goal was set, because
    // the goal loop runs entirely inside `KodEngine::process_goal_
    // streaming` and does not report per-turn progress back to the
    // TUI. Rather than keep a fake counter, the header now renders the
    // goal text (see `KodApp::goal`). If per-turn progress is ever
    // plumbed through, this method should return real numbers.

    // Agent management
    // Tool execution
    /// Placeholder body of a tool row while its call is still running.
    /// Completion (or cancel/fail) replaces it in place.
    const LIVE_TOOL_BODY_PLACEHOLDER: &'static str = "running…";

    // Streaming response
    // Session context accounting + auto-compact

    // Skills visible to the session

    // Slash-command completion (first token only — once there's a space,
    // the user is typing arguments, where path/model completion takes over).

    // Filesystem path completion for argument tokens:
    // `@src/ma`, `read ./Cargo`, `open ~/Doc`, `diff a/b|c`,
    // `read "my docs/rep`. Quotes and spaces inside quotes are honored.
    // Triggers on the last whitespace-separated token when it looks like
    // a path (leading `@`, or contains `/`, or starts with `.` / `~`).

    // ---- Swarm runs ----

    // Persistence: prompt history + session restore.
    //
    // Prompt history lives in `~/.kod/tui_history.json` (cap 500) and is
    // loaded at startup / appended on submit — power users keep history
    // across restarts. Sessions (chat messages) persist to
    // `~/.kod/tui_session.json` so a restart resumes where you left off.
    // `KOD_TUI_STATE_DIR` overrides the directory (tests use it).

    // Quit handling
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

/// Approval dialog state.
impl KodApp {
}

/// Notification-bell toggle.
impl KodApp {
}

/// Auto-compaction toggle.
impl KodApp {
}

/// Mid-session system prompt override.
impl KodApp {
}

/// Whole-transcript replacement (`/load`).
impl KodApp {
}

/// Fork helpers (`/fork`).
impl KodApp {
}

/// Reset transient UI state without touching chat or memory.
impl KodApp {
}

/// HTML export helpers (`/export-html`).
impl KodApp {
}

/// Minimal HTML escape. Covers `&`, `<`, `>`, `"`, and `'`.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// File-attachment state.
impl KodApp {
}

/// Question-dialog state, alongside the approval dialog.
impl KodApp {
}

/// Turn-completion notification.
impl KodApp {
}

/// Session-editing helpers used by /regenerate, /delete, /export.
impl KodApp {
}

impl Default for KodApp {
    fn default() -> Self {
        Self::new()
    }
}

// Widget-support accessors: thin views over state so the ui/ modules stay
// rendering-only (theme, phase, search, confirm, clipboard helpers).
impl KodApp {
    /// Add a call's USD cost to the session total. Negative and NaN
    /// values are rejected — a broken pricing block should not
    /// corrupt the accumulator.
    pub fn note_session_cost(&mut self, cost_usd: f64) {
        if cost_usd.is_finite() && cost_usd >= 0.0 {
            self.session_cost_usd += cost_usd;
        }
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
    pub fn note_session_usage(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        self.session_input_tokens = self.session_input_tokens.saturating_add(prompt_tokens);
        self.session_output_tokens = self.session_output_tokens.saturating_add(completion_tokens);
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
            SearchStatus::Editing => {
                format!(" /{} — typing… (Esc exits)", self.search_query_text())
            }
            SearchStatus::NoMatches => {
                format!(" /{} — no matches (Esc exits)", self.search_query_text())
            }
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
mod tests;
