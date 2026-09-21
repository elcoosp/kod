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

    pub fn is_selecting_hunks(&self) -> bool {
        self.pending_hunk_selection.is_some()
    }

    pub fn hunk_selection(&self) -> Option<&PendingHunkSelection> {
        self.pending_hunk_selection.as_ref()
    }

    /// Begin hunk selection for the current approval item. No-op when
    /// there is no current item, or the item is not a `patch_file`
    /// call with a non-empty `patch` argument.
    pub fn begin_hunk_selection(&mut self) -> bool {
        let Some(batch) = self.pending_batch.as_ref() else {
            return false;
        };
        let Some(item) = batch.current_item() else {
            return false;
        };
        if item.tool_name != "patch_file" {
            return false;
        }
        let Some(patch) = item.arguments.get("patch").and_then(|v| v.as_str()) else {
            return false;
        };
        let (header, hunks) = split_hunks(patch);
        if hunks.is_empty() {
            return false;
        }
        let selected = vec![true; hunks.len()];
        self.pending_hunk_selection = Some(PendingHunkSelection {
            approval_id: item.id,
            original_arguments: item.arguments.clone(),
            header,
            hunks,
            selected,
            cursor: 0,
        });
        true
    }

    pub fn cancel_hunk_selection(&mut self) {
        self.pending_hunk_selection = None;
    }

    pub fn hunk_toggle(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.toggle_current();
        }
    }

    pub fn hunk_next(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.advance();
        }
    }

    pub fn hunk_prev(&mut self) {
        if let Some(h) = self.pending_hunk_selection.as_mut() {
            h.retreat();
        }
    }

    /// Finish hunk selection: apply the filtered patch to the item's
    /// `arguments.patch`, and return `(id, arguments)` for the caller
    /// to send as an `ApproveWith`. `None` when the selection was
    /// cancelled or the state was inconsistent.
    pub fn hunk_commit(&mut self) -> Option<(u64, serde_json::Value)> {
        let sel = self.pending_hunk_selection.take()?;
        if sel.selected_count() == 0 {
            // Nothing to apply — refuse. The caller sees None and
            // keeps the dialog up.
            self.pending_hunk_selection = Some(sel);
            return None;
        }
        let filtered = sel.build_patch();
        let mut args = sel.original_arguments.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.insert("patch".to_string(), serde_json::Value::String(filtered));
        }
        // Apply to the pending item so the dialog reflects the change.
        if let Some(batch) = self.pending_batch.as_mut()
            && let Some(item) = batch.items.iter_mut().find(|i| i.id == sel.approval_id)
        {
            item.arguments = args.clone();
        }
        Some((sel.approval_id, args))
    }

    // --- Approval edit modal (Tier 2.3) --------------------------------

    pub fn is_editing_approval(&self) -> bool {
        self.pending_edit.is_some()
    }

    pub fn edit_buffer(&self) -> Option<&str> {
        self.pending_edit.as_ref().map(|e| e.buffer.as_str())
    }

    pub fn begin_edit_current_approval(&mut self) {
        let Some(batch) = self.pending_batch.as_ref() else {
            return;
        };
        let Some(item) = batch.current_item() else {
            return;
        };
        let buffer =
            serde_json::to_string_pretty(&item.arguments).unwrap_or_else(|_| "{}".to_string());
        self.pending_edit = Some(PendingEdit {
            approval_id: item.id,
            original: item.arguments.clone(),
            buffer,
        });
    }

    pub fn cancel_edit(&mut self) {
        self.pending_edit = None;
    }

    pub fn edit_push_char(&mut self, c: char) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.push(c);
        }
    }

    pub fn edit_push_newline(&mut self) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.push('\n');
        }
    }

    pub fn edit_backspace(&mut self) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.pop();
        }
    }

    /// Parse the buffer; on success, return `(id, arguments)` and
    /// leave the modal closed. On parse failure the modal stays open
    /// and `None` is returned.
    pub fn edit_commit(&mut self) -> Option<(u64, serde_json::Value)> {
        let Some(edit) = self.pending_edit.as_ref() else {
            return None;
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&edit.buffer) else {
            return None;
        };
        let id = edit.approval_id;
        // Apply to the pending item so the dialog shows the new args.
        if let Some(batch) = self.pending_batch.as_mut()
            && let Some(item) = batch.items.iter_mut().find(|i| i.id == id)
        {
            item.arguments = parsed.clone();
        }
        self.pending_edit = None;
        Some((id, parsed))
    }

    pub fn palette(&self) -> Option<&CommandPaletteState> {
        self.palette.as_ref()
    }

    pub fn is_palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// Open the palette. Idempotent — a second call while already
    /// open is a no-op, so Ctrl+K twice does not reset the query.
    pub fn open_palette(&mut self) {
        if self.palette.is_none() {
            self.palette = Some(CommandPaletteState::new());
        }
    }

    pub fn close_palette(&mut self) {
        self.palette = None;
    }

    /// Push a character into the query.
    pub fn palette_push_char(&mut self, c: char) {
        if let Some(p) = self.palette.as_mut() {
            p.query.push(c);
            p.selected = 0;
        }
    }

    pub fn palette_backspace(&mut self) {
        if let Some(p) = self.palette.as_mut() {
            p.query.pop();
            p.selected = 0;
        }
    }

    /// Move the selection down, wrapping.
    pub fn palette_next(&mut self) {
        let len = self.palette_candidates().len();
        if len == 0 {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.selected = (p.selected + 1) % len;
        }
    }

    /// Move the selection up, wrapping.
    pub fn palette_prev(&mut self) {
        let len = self.palette_candidates().len();
        if len == 0 {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.selected = (p.selected + len - 1) % len;
        }
    }

    /// The palette's currently-filtered entries.
    pub fn palette_candidates(&self) -> Vec<CommandPaletteEntry> {
        match self.palette.as_ref() {
            Some(p) => p.filtered(&build_palette_entries()),
            None => Vec::new(),
        }
    }

    pub fn palette_query(&self) -> Option<&str> {
        self.palette.as_ref().map(|p| p.query.as_str())
    }

    pub fn palette_selected(&self) -> usize {
        self.palette.as_ref().map(|p| p.selected).unwrap_or(0)
    }

    /// The currently-selected entry, if any.
    pub fn palette_selected_entry(&self) -> Option<CommandPaletteEntry> {
        let candidates = self.palette_candidates();
        let idx = self.palette_selected();
        candidates.get(idx).cloned()
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

    /// Set the pinned flag on the message at `idx` (0-based index into
    /// the full message list, so tool and system rows count). Returns
    /// `true` when the index was in range.
    ///
    /// The engine keeps its own copy of the pin state (a turn's
    /// `metadata.pinned`); the TUI mirrors it here so the display
    /// stays in sync without a round-trip. The two can drift if the
    /// engine compacts a turn the TUI still shows; the pin marker is
    /// cosmetic in that case — the model still gets the pinned text.
    pub fn set_message_pinned_at(&mut self, idx: usize, pinned: bool) -> bool {
        match self.messages.get_mut(idx) {
            Some(m) => {
                m.metadata.pinned = pinned;
                true
            }
            None => false,
        }
    }

    /// True when the message at `idx` is pinned.
    pub fn is_message_pinned(&self, idx: usize) -> bool {
        self.messages
            .get(idx)
            .map(|m| m.metadata.pinned)
            .unwrap_or(false)
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
        // Session cost accumulates per session, and `/clear` starts
        // a new session: the previous spend is no longer this
        // session's.
        self.session_cost_usd = 0.0;
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
                // The actual memory+checkpoint clear happens in the
                // main loop, which owns the engine handle. Resolving
                // here just clears the visible chat.
                ConfirmKind::ClearAll => self.clear_messages(),
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
        if !self.autocompact_enabled {
            return;
        }
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

    /// The markdown render cache. The chat widget reads it to
    /// avoid re-parsing unchanged messages on every frame; the
    /// engine does not touch it. Exposed as an `Arc` so a
    /// caller (a future sidebar widget, a test) can hold a
    /// reference without borrow-checker gymnastics.
    pub fn render_cache(&self) -> &std::sync::Arc<crate::markdown::RenderCache> {
        &self.render_cache
    }

    /// The live swarm-agent views, keyed by id. Read by the agent
    /// panel (D4-D5) and any future status surface.
    pub fn swarm_agents(&self) -> &std::collections::HashMap<kod_types::AgentId, SwarmAgentView> {
        &self.swarm_agents
    }

    /// The agent id at 1-based position `n` in the current run's start
    /// order, or `None` when there is no such agent. Backs the `@N`
    /// focus syntax in the input box: `@2 do X` finds the second agent
    /// that began work this run and steers it.
    pub fn swarm_agent_by_index(&self, n: usize) -> Option<&kod_types::AgentId> {
        if n == 0 {
            return None;
        }
        self.swarm_agent_order.get(n - 1)
    }

    /// Prepare for a new swarm run: clears the live-agent map so a
    /// previous run's rows are not appended to.
    pub fn begin_swarm(&mut self) {
        self.swarm_agents.clear();
        self.swarm_agent_order.clear();
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
        model: Option<String>,
    ) {
        let header = format!("{name} — {}", subtask.lines().next().unwrap_or(subtask));
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
            id.clone(),
            SwarmAgentView {
                message_id: msg_id,
                finished: false,
                name: name.to_string(),
                subtask: subtask.lines().next().unwrap_or(subtask).to_string(),
                model,
                worktree: None,
                branch: None,
                tool_count: 0,
                failure: None,
                retry_note: None,
            },
        );
        self.swarm_agent_order.push(id);
    }

    /// Append a text chunk to a live agent's row.
    pub fn swarm_agent_chunk(&mut self, id: &kod_types::AgentId, text: &str) {
        // A chunk starting with "  [tool: " is a tool-start notice the
        // engine emits; its count is what the panel shows as
        // "N tools".
        let is_tool_marker = text.starts_with("  [tool: ");
        let msg_id = {
            let Some(view) = self.swarm_agents.get_mut(id) else {
                return;
            };
            if view.finished {
                return;
            }
            if is_tool_marker {
                view.tool_count += 1;
            }
            view.message_id.clone()
        };
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
        view.failure = None;
        view.retry_note = None;
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

    /// Attach worktree info to a live agent's view (D4-D5).
    pub fn swarm_set_worktree(
        &mut self,
        id: &kod_types::AgentId,
        path: std::path::PathBuf,
        branch: String,
    ) {
        if let Some(view) = self.swarm_agents.get_mut(id) {
            view.worktree = Some(path);
            view.branch = Some(branch);
        }
    }

    /// Record that an agent is retrying (D4-D5).
    pub fn swarm_set_retrying(
        &mut self,
        id: &kod_types::AgentId,
        attempt: u32,
        max_attempts: u32,
        previous_error: &str,
    ) {
        if let Some(view) = self.swarm_agents.get_mut(id) {
            view.retry_note = Some(format!(
                "retrying ({}/{}): {}",
                attempt,
                max_attempts,
                previous_error.lines().next().unwrap_or(""),
            ));
        }
    }

    /// Mark a live row as failed and replace its buffer with the error.
    pub fn swarm_agent_failed(&mut self, id: &kod_types::AgentId, error: &str) {
        let Some(view) = self.swarm_agents.get_mut(id) else {
            return;
        };
        view.failure = Some(error.to_string());
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

        // Tier 3.2 — wrap the array in a `SessionSnapshot` so a
        // future schema bump is detectable. The `.messages` field is
        // byte-identical to the legacy array, so the file is not
        // substantially larger.
        let snapshot_owned = snapshot.to_vec();
        let wrapper = SessionSnapshot {
            schema_version: SESSION_SCHEMA_VERSION,
            messages: snapshot_owned,
        };
        let Ok(raw) = serde_json::to_string(&wrapper) else {
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
        // Tier 3.2 — accept both the current wrapper shape and the
        // pre-3.2 bare-array shape. The wrapper is tried first; a
        // bare array fails its `messages` field and falls through.
        let mut msgs: Vec<Message> = match serde_json::from_str::<SessionSnapshot>(&raw) {
            Ok(snap) => {
                // A future file's schema_version we do not know: the
                // loader tolerates it (messages parse) but logs so a
                // downgrade is visible.
                if snap.schema_version > SESSION_SCHEMA_VERSION {
                    tracing::warn!(
                        found = snap.schema_version,
                        expected = SESSION_SCHEMA_VERSION,
                        "tui_session.json was written by a newer kod; loading best-effort",
                    );
                }
                snap.messages
            }
            Err(_) => match serde_json::from_str::<Vec<Message>>(&raw) {
                Ok(v) => v,
                Err(_) => return 0,
            },
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

/// Approval dialog state.
impl KodApp {
    /// Install a batch of pending approvals and show the dialog.
    pub fn set_pending_batch(&mut self, batch: PendingApprovalBatch) {
        self.pending_batch = Some(batch);
    }

    /// The pending batch, if any.
    pub fn pending_batch(&self) -> Option<&PendingApprovalBatch> {
        self.pending_batch.as_ref()
    }

    /// Mutable access, used by the key handler to advance through
    /// items without rebuilding the batch.
    pub fn pending_batch_mut(&mut self) -> Option<&mut PendingApprovalBatch> {
        self.pending_batch.as_mut()
    }

    /// The current item of the pending batch, if any. Provided as a
    /// convenience for the widget, which only ever renders one item
    /// at a time; the batch itself is exposed by `pending_batch()`.
    pub fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_batch.as_ref().and_then(|b| b.current_item())
    }

    /// Clear the dialog. Called after every item is decided or Esc
    /// aborts the remainder.
    pub fn clear_pending_approval(&mut self) {
        self.pending_batch = None;
    }

    /// True when the approval dialog is up and normal key handling
    /// must be routed to it instead of the input box.
    pub fn is_approving(&self) -> bool {
        self.pending_batch.is_some()
    }
}

/// Notification-bell toggle.
impl KodApp {
    pub fn set_notify_bell(&mut self, enabled: bool) {
        self.notify_bell_enabled = enabled;
    }
    pub fn notify_bell(&self) -> bool {
        self.notify_bell_enabled
    }
}

/// Auto-compaction toggle.
impl KodApp {
    pub fn set_autocompact(&mut self, enabled: bool) {
        self.autocompact_enabled = enabled;
    }
    pub fn autocompact(&self) -> bool {
        self.autocompact_enabled
    }
}

/// Mid-session system prompt override.
impl KodApp {
    pub fn set_session_system_prompt(&mut self, text: String) {
        self.session_system_prompt = Some(text);
    }
    pub fn clear_session_system_prompt(&mut self) {
        self.session_system_prompt = None;
    }
    pub fn session_system_prompt(&self) -> Option<&str> {
        self.session_system_prompt.as_deref()
    }
}

/// Whole-transcript replacement (`/load`).
impl KodApp {
    /// Replace the current chat with `messages`, resetting `sequence`
    /// so the widget's sort is a no-op. Refuses to add a `sequence`
    /// value past the existing `next_seq`.
    pub fn replace_messages(&mut self, mut messages: Vec<Message>) {
        for (i, m) in messages.iter_mut().enumerate() {
            m.sequence = i as u64;
        }
        self.next_seq = messages.len() as u64;
        self.messages = messages;
        self.scroll_to_bottom();
    }
}

/// Fork helpers (`/fork`).
impl KodApp {
    /// Save the current chat as a restorable fork. The current chat is
    /// left in place; `/undo` will later restore this saved copy. A
    /// subsequent `/clear` drops the live chat and leaves the fork on
    /// `cleared_stack`, giving the user a way back.
    pub fn fork_messages(&mut self) -> usize {
        if self.messages.is_empty() {
            return 0;
        }
        let snapshot = self.messages.clone();
        let n = snapshot.len();
        self.cleared_stack.push(snapshot);
        // Keep the stack bounded.
        while self.cleared_stack.len() > 5 {
            self.cleared_stack.remove(0);
        }
        n
    }

    /// Number of saved forks available via /undo.
    pub fn fork_count(&self) -> usize {
        self.cleared_stack.len()
    }
}

/// Reset transient UI state without touching chat or memory.
impl KodApp {
    /// Clear everything that is not the chat itself: the input box,
    /// search state, tool expansion, attachments, dialogs, history
    /// index, and completion state. Returns a count of the fields that
    /// were actually non-empty before the reset, so a caller can tell
    /// the user what was cleared.
    ///
    /// Distinct from `/clear` (which wipes the chat and the engine
    /// transcript) and `/clearall` (which also wipes memory and
    /// checkpoints). This one is the "reset my terminal to a clean
    /// state" — no data is discarded.
    pub fn reset_transient_state(&mut self) -> usize {
        let mut cleared = 0usize;
        if !self.input.is_empty() {
            cleared += 1;
        }
        if !self.question_input.is_empty() {
            cleared += 1;
        }
        if self.search_query.is_some() {
            cleared += 1;
        }
        if !self.expanded_tools.is_empty() {
            cleared += 1;
        }
        if self.pending_batch.is_some() {
            cleared += 1;
        }
        if self.pending_question.is_some() {
            cleared += 1;
        }
        if !self.attached_files.is_empty() {
            cleared += 1;
        }
        if self.last_error.is_some() {
            cleared += 1;
        }

        self.clear_input();
        self.clear_search();
        self.expanded_tools.clear();
        self.pending_batch = None;
        self.pending_question = None;
        self.question_input.clear();
        self.attached_files.clear();
        self.last_error = None;
        self.completion_index = 0;
        self.history_index = None;
        self.draft.clear();
        self.set_input_mode(InputMode::Normal);
        self.mode = AppMode::Normal;

        cleared
    }
}

/// HTML export helpers (`/export-html`).
impl KodApp {
    /// Render the current chat as a self-contained HTML document with
    /// inline CSS. No external assets, no scripts — the output is a
    /// single file a user can email or drop in a wiki.
    pub fn export_html(&self) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
        out.push_str("<title>KOD session</title>");
        out.push_str("<style>");
        out.push_str("body{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;line-height:1.5;color:#111;background:#fff}");
        out.push_str(".msg{margin:1.5rem 0;padding:.75rem 1rem;border-left:3px solid #ddd;background:#fafafa}");
        out.push_str(".you{border-left-color:#3b82f6}");
        out.push_str(".ai{border-left-color:#10b981}");
        out.push_str(".sys{border-left-color:#f59e0b;color:#555}");
        out.push_str(
            ".tool{border-left-color:#a855f7;font-family:ui-monospace,monospace;font-size:.9em}",
        );
        out.push_str(".agent{border-left-color:#ec4899}");
        out.push_str(".role{font-size:.8em;text-transform:uppercase;letter-spacing:.05em;color:#666;margin-bottom:.25rem}");
        out.push_str(".time{font-size:.75em;color:#999;margin-left:.5rem}");
        out.push_str("pre{white-space:pre-wrap;word-wrap:break-word;margin:0}");
        out.push_str("</style></head><body>\n");
        out.push_str("<h1>KOD session</h1>\n");

        let mut ordered: Vec<&Message> = self.messages.iter().collect();
        ordered.sort_by_key(|m| m.sequence);
        for m in ordered {
            let (cls, label) = match &m.role {
                kod_types::MessageRole::User => ("you", "you"),
                kod_types::MessageRole::Assistant => ("ai", "ai"),
                kod_types::MessageRole::System => ("sys", "sys"),
                kod_types::MessageRole::Tool => ("tool", "tool"),
                kod_types::MessageRole::Agent(_) => ("agent", "agent"),
            };
            let escaped = html_escape(&m.content);
            out.push_str(&format!(
                "<div class=\"msg {}\"><div class=\"role\">{}<span class=\"time\">{}</span></div><pre>{}</pre></div>\n",
                cls,
                label,
                m.timestamp.format("%Y-%m-%d %H:%M:%S"),
                escaped,
            ));
        }
        out.push_str("</body></html>\n");
        out
    }
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
    /// Add a file to the pending attachment list. Returns false when
    /// the file cannot be read — the caller can then print a message.
    pub fn attach_file(&mut self, path: std::path::PathBuf) -> bool {
        if !path.is_file() {
            return false;
        }
        if self.attached_files.contains(&path) {
            return true; // idempotent
        }
        self.attached_files.push(path);
        true
    }

    /// Take and clear the attachments, returning the file list.
    pub fn take_attachments(&mut self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.attached_files)
    }

    /// The currently attached files (for display).
    pub fn attached_files(&self) -> &[std::path::PathBuf] {
        &self.attached_files
    }

    /// Clear all attachments.
    pub fn clear_attachments(&mut self) {
        self.attached_files.clear();
    }
}

/// Question-dialog state, alongside the approval dialog.
impl KodApp {
    pub fn set_pending_question(&mut self, q: PendingQuestion) {
        self.pending_question = Some(q);
        self.question_input.clear();
    }
    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_question.as_ref()
    }
    pub fn clear_pending_question(&mut self) -> String {
        let text = std::mem::take(&mut self.question_input);
        self.pending_question = None;
        text
    }
    pub fn is_asking(&self) -> bool {
        self.pending_question.is_some()
    }
    pub fn question_input(&self) -> &str {
        &self.question_input
    }
    pub fn question_input_mut(&mut self) -> &mut String {
        &mut self.question_input
    }
}

/// Turn-completion notification.
impl KodApp {
    /// If the turn that just finished ran longer than `threshold`, ring
    /// the terminal bell and (on supporting terminals) send an OSC 9
    /// notification. Returns the elapsed time when a bell fired, so
    /// the caller can print an informational line.
    ///
    /// Bells are cheap and silent on most setups; a threshold keeps
    /// them from firing on every trivial turn. 30 seconds is
    /// conservative — a user can react, but not every keystroke gets a
    /// beep.
    pub fn notify_turn_complete(
        &mut self,
        threshold: std::time::Duration,
    ) -> Option<std::time::Duration> {
        if !self.notify_bell_enabled {
            // Still clear the timestamp so the state does not leak
            // into the next turn's measurement.
            self.turn_started_at = None;
            return None;
        }
        let started = self.turn_started_at.take()?;
        let elapsed = started.elapsed();
        if elapsed < threshold {
            return None;
        }
        // Terminal bell.
        eprint!("\x07");
        // OSC 9 notification (iTerm2, Windows Terminal, kitty). Some
        // terminals do not implement it and ignore the sequence.
        let msg = format!("kod: turn completed in {}s", elapsed.as_secs());
        eprint!("\x1b]9;{}\x07", msg);
        Some(elapsed)
    }
}

/// Session-editing helpers used by /regenerate, /delete, /export.
impl KodApp {
    /// Remove the trailing (assistant, ...) pair through the preceding
    /// user message. Returns the removed user text when something was
    /// removed, so the caller can feed it back into a new turn.
    pub fn drop_last_exchange(&mut self) -> Option<String> {
        // Walk back from the end: drop tool / assistant / system
        // messages, then the user message, then stop.
        let mut user_text: Option<String> = None;
        while let Some(msg) = self.messages.last() {
            match msg.role {
                kod_types::MessageRole::User => {
                    user_text = Some(msg.content.clone());
                    self.messages.pop();
                    break;
                }
                kod_types::MessageRole::Assistant
                | kod_types::MessageRole::Tool
                | kod_types::MessageRole::System => {
                    self.messages.pop();
                }
                kod_types::MessageRole::Agent(_) => {
                    self.messages.pop();
                }
            }
        }
        user_text
    }

    /// Render the current chat as Markdown. Same shape the CLI's
    /// `kod sessions export --format markdown` produces, so the two
    /// paths agree.
    pub fn export_markdown(&self) -> String {
        let mut out = String::from("# KOD session\n\n");
        let mut ordered: Vec<&Message> = self.messages.iter().collect();
        ordered.sort_by_key(|m| m.sequence);
        for m in ordered {
            let label = match &m.role {
                kod_types::MessageRole::User => "you",
                kod_types::MessageRole::Assistant => "ai",
                kod_types::MessageRole::System => "sys",
                kod_types::MessageRole::Tool => "tool",
                kod_types::MessageRole::Agent(_) => "agent",
            };
            out.push_str(&format!(
                "## {} · {}\n\n",
                label,
                m.timestamp.format("%Y-%m-%d %H:%M:%S")
            ));
            let fenced = matches!(
                m.role,
                kod_types::MessageRole::Assistant | kod_types::MessageRole::Tool
            );
            if fenced {
                out.push_str("```\n");
                out.push_str(&m.content);
                if !m.content.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            } else {
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
        }
        out
    }
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

    /// Milliseconds from `begin_generation` to the first non-empty
    /// streamed text chunk of the current turn. `None` before that
    /// chunk lands, and after the turn ends (both `finish_response`
    /// and `fail_generation` clear it).
    ///
    /// Wall-clock measured on the TUI thread, not by the engine.
    /// The two are close enough for a status-bar figure; if the
    /// engine ever wants to make this exact (provider-reported
    /// timing, sub-millisecond precision), it should emit a
    /// dedicated event, not have the TUI measure a proxy.
    /// The current turn's streaming rate in tokens-per-second, if a
    /// rate can be measured. `None` before the second chunk arrives
    /// (a single chunk yields a zero duration, which is a division by
    /// zero, and is also not a meaningful measurement), and after the
    /// turn ends.
    ///
    /// The figure is `(streamed_chars / 4) / seconds-since-first-chunk`
    /// — the same 4-chars-per-token approximation the rest of the TUI
    /// uses. A local model at ~30 tokens/sec shows ~30; a stalled
    /// stream decays toward 0 as the elapsed time grows without a
    /// fresh chunk. The status widget reads it once per frame.
    pub fn tokens_per_sec(&self) -> Option<f32> {
        let first = self.first_chunk_at?;
        let last = self.last_chunk_at?;
        // Require at least two chunks' worth of elapsed time so the
        // first-chunk case does not produce a divide-by-zero or an
        // absurd instantaneous rate.
        let elapsed = last.duration_since(first).as_secs_f32();
        if elapsed < 0.05 {
            return None;
        }
        let tokens = self.streamed_chars_this_turn as f32 / 4.0;
        Some(tokens / elapsed)
    }

    pub fn ttft_ms(&self) -> Option<u128> {
        let started = self.spinner_started?;
        let first = self.first_chunk_at?;
        Some(first.duration_since(started).as_millis())
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

    /// Whether the effective network access is enabled. Drives the
    /// header's `net:on` badge (D3-C5). The TUI sets this from the
    /// engine's `network_access_setting()` after init; the default is
    /// off, matching `LlmConfig::network_access = false`.
    pub fn network_access_enabled(&self) -> bool {
        self.network_access_enabled
    }

    /// Setter used by `TuiLoop::init_engine`.
    /// The effective sandbox label, or empty when no engine has
    /// been initialized yet. Callers should render a badge only
    /// for a non-empty value.
    pub fn sandbox_label(&self) -> &str {
        &self.sandbox_label
    }

    /// Set the sandbox label. Called by `TuiLoop::init_engine`
    /// once the engine is up and the resolver's choice is known.
    pub fn set_sandbox_label(&mut self, label: String) {
        self.sandbox_label = label;
    }

    pub fn set_network_access_enabled(&mut self, enabled: bool) {
        self.network_access_enabled = enabled;
    }

    /// Total input tokens the provider has reported this session.
    pub fn session_input_tokens(&self) -> usize {
        self.session_input_tokens
    }

    /// Total output tokens the provider has reported this session.
    pub fn session_output_tokens(&self) -> usize {
        self.session_output_tokens
    }

    /// USD spent so far this session, per the pricing the endpoints
    /// reported. 0.0 when no endpoint has a `[pricing]` block — the
    /// caller checks `cost_known()` before rendering a `$` figure,
    /// so a genuine 0.0 from a free local model is distinguishable
    /// from "pricing not configured" only by the second flag, not
    /// by this number.
    pub fn session_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }

    /// True once at least one reported call has contributed a cost.
    /// The header renders the `$` figure only when this is true, so
    /// an endpoint without pricing never produces a fake `$0.00`.
    pub fn cost_known(&self) -> bool {
        self.session_cost_usd > 0.0
    }

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
