//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, memory, and swarm
//! to process user requests end-to-end.

use crate::router::{RouterConfig, TaskResponse, TaskRouter};
use kod_error::{KodError, Result};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_tools::{
    ExecuteCommandTool, FileInfoTool, GrepTool, ListFilesTool, ReadFileTool, ToolContext,
    ToolRegistry, WriteFileTool,
};
use kod_types::{ToolCall, ToolDefinition, ToolPermissions, ToolResult};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;

/// Max agentic tool rounds per `process()` call before forcing a summary.
const MAX_TOOL_ROUNDS: usize = 150;
/// Max turns of the `/goal` loop before it stops and reports progress.
const MAX_GOAL_TURNS: usize = 6;

/// Marker prefix for tool-start notices inside the `process_streaming`
/// chunk channel: `\0kod-tool:<name>\0`. The TUI turns these into its
/// "running …" indicator instead of chat text (see `parse_tool_start`).
pub const TOOL_START_MARKER: &str = "\0kod-tool:";

/// Build a tool-start marker chunk for `name`.
pub fn tool_start_marker(name: &str) -> String {
    format!("{TOOL_START_MARKER}{name}\0")
}

/// If `chunk` is a tool-start marker, return the tool name.
pub fn parse_tool_start(chunk: &str) -> Option<&str> {
    chunk.strip_prefix(TOOL_START_MARKER)?.strip_suffix('\0')
}

/// Marker prefix for tool-argument excerpts inside the
/// `process_streaming` chunk channel: `\0kod-args:<one-line display>\0`.
/// Sent after a streamed call's arguments are assembled but before the
/// tool executes, so the TUI's "running …" line can show the actual
/// command/file instead of just the tool name.
pub const TOOL_ARGS_MARKER: &str = "\0kod-args:";

/// Build a tool-args marker chunk carrying a one-line display string.
pub fn tool_args_marker(display: &str) -> String {
    format!("{TOOL_ARGS_MARKER}{display}\0")
}

/// If `chunk` is a tool-args marker, return the one-line display string.
pub fn parse_tool_args(chunk: &str) -> Option<&str> {
    chunk.strip_prefix(TOOL_ARGS_MARKER)?.strip_suffix('\0')
}

/// Marker prefix for per-tool completion notices inside the
/// `process_streaming` chunk channel:
/// `\0kod-done:<header>\0<summary>\0<duration_ms>`.
/// Sent by [`KodEngine::run_streaming_loop`] the moment each tool call
/// finishes — long before the whole agentic loop returns — so the TUI
/// can fill the live "running …" row in immediately instead of batching
/// every `ToolCompleted` at task end. Headers/summaries are sanitized
/// (no `\0`) when built by [`tool_done_marker`].
pub const TOOL_DONE_MARKER: &str = "\0kod-done:";

/// Build a tool-done marker chunk for one finished call.
pub fn tool_done_marker(header: &str, summary: &str, duration_ms: u64) -> String {
    let clean = |s: &str| s.replace('\0', " ");
    format!(
        "{TOOL_DONE_MARKER}{}\0{}\0{duration_ms}",
        clean(header),
        clean(summary)
    )
}

/// If `chunk` is a tool-done marker, return `(header, summary,
/// duration_ms)`. A malformed duration degrades to `0` rather than
/// dropping the completion.
pub fn parse_tool_done(chunk: &str) -> Option<(&str, &str, u64)> {
    let rest = chunk.strip_prefix(TOOL_DONE_MARKER)?;
    let mut parts = rest.splitn(3, '\0');
    let header = parts.next()?;
    let summary = parts.next()?;
    let duration = parts.next()?;
    Some((header, summary, duration.parse::<u64>().unwrap_or(0)))
}

/// Short human duration for tool rows: `340ms`, `1.2s`, `1m05s`.
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Marker for the post-tool thinking phase: tool result was reinjected
/// and the LLM is reasoning again. The TUI switches from "tool: …" back
/// to "thinking…" so a slow reinjection doesn't look like a stuck tool.
pub const THINKING_MARKER: &str = "\0kod-thinking\0";

pub fn thinking_marker() -> String {
    THINKING_MARKER.to_string()
}

pub fn is_thinking_marker(chunk: &str) -> bool {
    chunk == THINKING_MARKER
}

/// One-line brief for a tool call: `execute_command cargo test …`,
/// `read_file path=…`. Used for the live "running" indicator.
pub fn format_call_brief(name: &str, args: &serde_json::Value) -> String {
    if name == "execute_command" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            // Multi-line shell snippets read as their first line only.
            let first = cmd.lines().next().unwrap_or(cmd).trim();
            let short = if first.len() > 100 {
                format!("{}…", &first[..100])
            } else {
                first.to_string()
            };
            if short.is_empty() {
                return name.to_string();
            }
            return format!("{name} {short}");
        }
        return name.to_string();
    }
    const KEYS: [&str; 4] = ["path", "pattern", "file", "content"];
    if let Some(obj) = args.as_object() {
        for key in KEYS {
            if let Some(v) = obj.get(key) {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let short = if s.len() > 80 {
                    format!("{}…", &s[..80])
                } else {
                    s
                };
                return format!("{name} {key}={short}");
            }
        }
    }
    // Fallback: show a short preview of the raw arguments so unknown tools
    // still carry context in the running indicator.
    let raw = args.to_string();
    if raw.is_empty() || raw == "null" || raw == "{}" || raw == "[]" {
        return name.to_string();
    }
    let short = if raw.len() > 80 {
        format!("{}...", &raw[..80])
    } else {
        raw
    };
    format!("{name} {short}")
}

/// Max result lines kept per tool message; the rest collapses to a counter.
pub const TOOL_RESULT_LINES: usize = 12;

/// Human-readable tool header: `list_files path=.` instead of raw JSON.
/// `complete_tool_execution` wraps it in `[...]`, so no brackets here.
pub fn format_tool_header(name: &str, args: &serde_json::Value) -> String {
    const KEYS: [&str; 5] = ["path", "pattern", "command", "file", "content"];
    let mut parts = vec![name.to_string()];
    if let Some(obj) = args.as_object() {
        for key in KEYS {
            if let Some(v) = obj.get(key) {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let short = if s.len() > 60 {
                    format!("{}…", &s[..60])
                } else {
                    s
                };
                // Long paths read better as their last two segments.
                let shown = if key == "path" || key == "file" {
                    shorten_path(&short)
                } else {
                    short
                };
                parts.push(format!("{key}={shown}"));
            }
        }
        if let Some(rec) = obj.get("recursive").and_then(|v| v.as_bool())
            && rec
        {
            parts.push("recursive".to_string());
        }
    }
    parts.join(" ")
}

/// Keep the tail of a long path: `/a/b/c` → `…/b/c`.
fn shorten_path(path: &str) -> String {
    const KEEP: usize = 2;
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= KEEP + 1 {
        return path.to_string();
    }
    segments = segments[segments.len() - KEEP..].to_vec();
    format!("…/{}", segments.join("/"))
}

/// Render a tool result for chat: file lists become counts + names,
/// command output keeps its lines, everything caps at [`TOOL_RESULT_LINES`]
/// with an explicit "…and N more" instead of a mid-token cut.
pub fn summarize_tool_result(name: &str, result: &ToolResult) -> String {
    match result {
        ToolResult::Success(v) => summarize_success(name, v),
        ToolResult::Error(e) => cap_lines(&format!("Error: {}", e.trim()), 5),
        ToolResult::RequiresConfirmation { description, .. } => {
            format!("Needs confirmation: {description}")
        }
    }
}

fn summarize_success(name: &str, v: &serde_json::Value) -> String {
    // read_file: path + size + short preview only. The full content still
    // reaches the model through the tool-result feedback block — the chat
    // row stays lean while the agent loses nothing.
    if name == "read_file"
        && let Some(content) = v.get("content").and_then(|s| s.as_str())
    {
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        let lines = content.lines().count();
        let mut out = format!(
            "{} · {} line{} · {} chars",
            path,
            lines,
            if lines == 1 { "" } else { "s" },
            content.len()
        );
        let preview: Vec<&str> = content.lines().take(3).collect();
        if !preview.is_empty() {
            out.push('\n');
            out.push_str(&preview.join("\n"));
            if lines > preview.len() {
                out.push_str("\n…");
            }
        }
        return out;
    }
    // list_files: {path, files:[...]} → count + names.
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        let dir = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_default();
        let shown: Vec<String> = files
            .iter()
            .take(TOOL_RESULT_LINES)
            .filter_map(|f| f.as_str())
            .map(|f| {
                // Strip the listed dir prefix; bare names scan fastest.
                let bare = f.strip_prefix(dir.as_str()).unwrap_or(f);
                let bare = bare.trim_start_matches('/');
                if bare.is_empty() {
                    f.to_string()
                } else {
                    bare.to_string()
                }
            })
            .map(|f| format!("· {f}"))
            .collect();
        let mut out = format!(
            "{} entr{} in {}:",
            files.len(),
            if files.len() == 1 { "y" } else { "ies" },
            if dir.is_empty() { name } else { &dir }
        );
        if !shown.is_empty() {
            out.push('\n');
            out.push_str(&shown.join("\n"));
        }
        if files.len() > shown.len() {
            out.push_str(&format!("\n… and {} more", files.len() - shown.len()));
        }
        return out;
    }
    // execute_command / read_file: {stdout, stderr} or {path, content}.
    if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
        let mut out = cap_lines(stdout.trim_end(), TOOL_RESULT_LINES);
        if let Some(stderr) = v.get("stderr").and_then(|s| s.as_str())
            && !stderr.trim().is_empty()
        {
            out.push_str(&format!("\nstderr:\n{}", cap_lines(stderr.trim_end(), 4)));
        }
        return if out.is_empty() {
            "(no output)".to_string()
        } else {
            out
        };
    }
    if let Some(content) = v.get("content").and_then(|s| s.as_str()) {
        let path = v.get("path").and_then(|p| p.as_str()).unwrap_or(name);
        return format!(
            "{}:\n{}",
            path,
            cap_lines(content.trim_end(), TOOL_RESULT_LINES)
        );
    }
    // Anything else: pretty JSON, capped by line (never mid-token).
    match serde_json::to_string_pretty(v) {
        Ok(pretty) => cap_lines(&pretty, TOOL_RESULT_LINES),
        Err(_) => cap_lines(&v.to_string(), TOOL_RESULT_LINES),
    }
}

/// Keep the first `max` lines; append an explicit remainder counter.
fn cap_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    format!(
        "{}\n… and {} more lines",
        lines[..max].join("\n"),
        lines.len() - max
    )
}

/// One remembered conversation turn. The engine is stateless per call by
/// default — without this, every prompt arrives as a "fresh conversation"
/// and any TUI trim wipes the model's memory mid-session.
#[derive(Debug, Clone)]
struct HistoryTurn {
    /// `true` = user, `false` = assistant.
    user: bool,
    text: String,
}

/// Cap the remembered transcript: last turns, each truncated, total render
/// capped so history can never blow the context window on its own.
const MAX_HISTORY_TURNS: usize = 40;
const MAX_TURN_CHARS: usize = 1500;
const MAX_HISTORY_CHARS: usize = 12_000;

/// Outcome of one tool-execution round: results for the response plus a
/// prompt block feeding them back to the model. `elapsed_ms` parallels
/// `results` — per-call wall time for the live done-markers.
struct ToolRound {
    results: Vec<ToolResult>,
    prompt_block: String,
    elapsed_ms: Vec<u64>,
}

/// Main engine for KOD
pub struct KodEngine {
    router: Arc<TaskRouter>,
    provider: RwLock<Option<Arc<dyn LlmProvider>>>,
    is_running: RwLock<bool>,
    tools: Arc<ToolRegistry>,
    tool_context: ToolContext,
    working_dir: PathBuf,
    /// Steer notes queued while a prompt is running (see [`KodEngine::steer`]).
    steer_queue: RwLock<Vec<String>>,
    /// Set by [`KodEngine::request_cancel`]; loops check it between rounds.
    cancelled: AtomicBool,
    /// Transcript of past turns, rendered into every prompt (see
    /// [`KodEngine::render_history`]). Survives TUI-side trims/compact —
    /// those only touch display messages, never this.
    history: RwLock<Vec<HistoryTurn>>,
}

impl KodEngine {
    /// Create a new engine
    pub fn new(config: RouterConfig, db_path: PathBuf) -> Result<Self> {
        let working_dir = config.working_dir.clone();
        let tool_context =
            ToolContext::new(working_dir.clone()).with_permissions(ToolPermissions {
                read_files: true,
                write_files: true,
                execute_commands: true,
                network_access: false,
                git_operations: false,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            });
        let router = TaskRouter::new(config, db_path)?;

        Ok(Self {
            router: Arc::new(router),
            provider: RwLock::new(None),
            is_running: RwLock::new(false),
            tools: Arc::new(ToolRegistry::new()),
            tool_context,
            working_dir,
            steer_queue: RwLock::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            history: RwLock::new(Vec::new()),
        })
    }

    /// Set the LLM provider
    pub async fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }

    /// List available models from the provider, if one is set.
    pub async fn list_models(&self) -> Vec<String> {
        let provider = self.provider.read().await;
        if let Some(p) = provider.as_ref() {
            match p.list_models().await {
                Ok(models) => models,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        "list models request failed — \
                         check provider base_url and API key"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    }

    /// Start the engine
    pub async fn start(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if *running {
            return Err(KodError::InvalidState("Engine already running".to_string()));
        }

        *running = true;

        // Register the built-in tools once (start runs exactly once —
        // second call errors above). Tools fail closed via ToolContext
        // permissions unless explicitly granted in `new()`.
        self.tools.register(Box::new(ReadFileTool::new())).await;
        self.tools.register(Box::new(WriteFileTool::new())).await;
        self.tools.register(Box::new(ListFilesTool::new())).await;
        self.tools.register(Box::new(GrepTool::new())).await;
        self.tools.register(Box::new(FileInfoTool::new())).await;
        self.tools
            .register(Box::new(ExecuteCommandTool::new()))
            .await;

        tracing::info!("KOD engine started");
        Ok(())
    }

    /// Process user input
    pub async fn process(&self, input: &str) -> Result<TaskResponse> {
        // Check if engine is running
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }

        // If we have a provider, use it to generate a response
        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {
            // Process through router for task classification and context
            let response = self.router.process_input(input).await?;

            // Build the full prompt using the router's context builder
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;

            // Ground the model: where it runs and what it can touch.
            // Without this it claims "no filesystem access" even though
            // tools are wired below.
            let definitions = self.tools.get_definitions().await;
            let convo = self.ground_prompt(prompt, &definitions);

            // Agentic loop: generate (with tools) -> execute -> feed back.
            let options = GenerationOptions::default();
            let mut pending = convo;
            let (final_text, tool_calls, tool_results, usage) = self
                .run_collected_loop(provider, &mut pending, &definitions, &options)
                .await?;
            // Model only called tools and never wrote back: ask for a summary.
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                pending.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                provider.generate(&pending, &options).await?
            } else {
                final_text
            };
            self.record_turn(false, &final_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                swarm_used: response.swarm_used,
                execution_time_ms: response.execution_time_ms,
                usage,
            });
        }

        // No provider — fall back to router's built-in handlers
        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Process user input, streaming text chunks live to `chunk_tx`.
    ///
    /// Same result as [`process`], but answer tokens arrive as they generate
    /// (the TUI renders each chunk immediately) and tool starts arrive as
    /// [`tool_start_marker`] chunks (see [`parse_tool_start`]) so the UI can
    /// show "running …" while the tool actually executes.
    pub async fn process_streaming(
        &self,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }

        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);

            let options = GenerationOptions::default();
            let (final_text, tool_calls, tool_results, usage) = self
                .run_streaming_loop(provider, &mut pending, &definitions, &options, chunk_tx)
                .await?;
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                pending.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                self.stream_summary(provider, &pending, &options, chunk_tx)
                    .await?
            } else {
                final_text
            };
            self.record_turn(false, &final_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                swarm_used: response.swarm_used,
                execution_time_ms: response.execution_time_ms,
                usage,
            });
        }

        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Work toward `goal` across turns until the model declares it met.
    ///
    /// Same streaming contract as [`process_streaming`], but after each
    /// agentic pass the conversation continues with a "keep going" nudge
    /// until the reply contains `GOAL MET` (case-insensitive),
    /// [`MAX_GOAL_TURNS`] passes run, or [`KodEngine::request_cancel`]
    /// fires. Steer notes queued via [`KodEngine::steer`] are injected
    /// every turn. Each turn's text streams live; turns are separated by
    /// a `—— turn N ——` marker chunk so the TUI can render progress.
    pub async fn process_goal_streaming(
        &self,
        input: &str,
        goal: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }

        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);
            pending.push_str(&format!(
                "\n## Goal\n\n{goal}\n\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\n"
            ));

            let options = GenerationOptions::default();
            let mut all_text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_results: Vec<ToolResult> = Vec::new();
            let mut last_usage: Option<kod_provider::TokenUsage> = None;
            for turn in 1..=MAX_GOAL_TURNS {
                if self.is_cancelled() {
                    return Err(KodError::InvalidState("cancelled by user".to_string()));
                }
                if turn > 1 {
                    let _ = chunk_tx.send(format!("\n\n—— turn {turn} ——\n")).await;
                    pending.push_str(
                        "\n\nContinue working toward the goal above. If it is now fully reached, reply with GOAL MET plus a short summary instead of calling more tools.\n",
                    );
                }
                self.apply_steers(&mut pending).await;
                let (final_text, calls, results, usage) = self
                    .run_streaming_loop(provider, &mut pending, &definitions, &options, chunk_tx)
                    .await?;
                last_usage = usage.or(last_usage);
                if !all_text.is_empty() && !final_text.trim().is_empty() {
                    all_text.push_str("\n\n");
                }
                all_text.push_str(&final_text);
                tool_calls.extend(calls);
                tool_results.extend(results);
                if final_text.to_uppercase().contains("GOAL MET") {
                    break;
                }
                if turn == MAX_GOAL_TURNS {
                    all_text.push_str("\n\n(Goal loop stopped after maximum turns — progress above. Refine with /goal or /steer.)");
                }
            }
            self.record_turn(false, &all_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(all_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                swarm_used: response.swarm_used,
                execution_time_ms: response.execution_time_ms,
                usage: last_usage,
            });
        }

        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Collected (non-streaming) agentic loop used by [`process`].
    async fn run_collected_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        for _ in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled() {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            match provider
                .generate_with_tools(pending, definitions, options)
                .await?
            {
                GenerationResponse::Text { content, usage } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {
                    last_usage = usage.or(last_usage);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
                GenerationResponse::Mixed {
                    content,
                    calls,
                    usage,
                } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes to the running conversation (each once).
    async fn apply_steers(&self, pending: &mut String) {
        for note in self.take_steers().await {
            pending.push_str(&format!(
                "\n\n## User steer (new instruction — adjust course now, do not restart what already worked)\n{note}\n"
            ));
        }
    }

    /// Streaming agentic loop: text chunks are forwarded to `chunk_tx` the
    /// moment they arrive; tool-start markers go through the same channel.
    async fn run_streaming_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        for _ in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled() {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            let (text, calls, usage) = self
                .stream_round(provider, pending, definitions, options, chunk_tx)
                .await?;
            last_usage = usage.or(last_usage);
            final_text.push_str(&text);
            if calls.is_empty() {
                break;
            }
            // The running indicator now shows what each call actually does
            // (`execute_command cargo test …`), not just the tool name.
            for call in &calls {
                let _ = chunk_tx
                    .send(tool_args_marker(&format_call_brief(
                        &call.tool_name,
                        &call.arguments,
                    )))
                    .await;
            }
            let section = self.run_tool_calls(&calls).await;
            // Each call finished: hand the TUI its completion live (header
            // + summary + wall time) so the "running …" row fills in now,
            // not when the whole loop returns. Markers travel the same
            // channel in call order; the task-end `ToolCompleted` events
            // remain as fallback and are idempotent there.
            for (call, (result, ms)) in calls
                .iter()
                .zip(section.results.iter().zip(section.elapsed_ms.iter()))
            {
                let header = format_tool_header(&call.tool_name, &call.arguments);
                let summary = summarize_tool_result(&call.tool_name, result);
                let _ = chunk_tx
                    .send(tool_done_marker(&header, &summary, *ms))
                    .await;
            }
            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\n\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// One streaming round: forward text live, assemble tool calls from
    /// `ToolCallStart`/`ToolCallDelta` framing.
    async fn stream_round(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &str,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        use futures::StreamExt;
        let mut stream = provider.stream_with_tools(pending, definitions, options);
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut cur_name: Option<String> = None;
        let mut cur_args = String::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        while let Some(item) = stream.next().await {
            match item? {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                }
                StreamChunk::ToolCallStart { name } => {
                    if let Some(prev) = cur_name.take() {
                        calls.push(finish_stream_call(prev, &cur_args));
                        cur_args.clear();
                    }
                    let _ = chunk_tx.send(tool_start_marker(&name)).await;
                    cur_name = Some(name);
                }
                StreamChunk::ToolCallDelta { arguments } => {
                    cur_args.push_str(&arguments);
                }
                StreamChunk::Usage(usage) => {
                    last_usage = Some(usage);
                }
                StreamChunk::Done => break,
            }
        }
        if let Some(prev) = cur_name.take() {
            calls.push(finish_stream_call(prev, &cur_args));
        }
        Ok((text, calls, last_usage))
    }

    /// Stream a plain-text summary (tools already ran): forwards chunks live.
    async fn stream_summary(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &str,
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<String> {
        use futures::StreamExt;
        let mut stream = provider.stream(pending, options);
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                }
                StreamChunk::Usage(_) => {
                    // Summary stream usage is not critical (already counted in tool rounds)
                }
                _ => {}
            }
        }
        Ok(text)
    }

    /// Append the environment + tool inventory grounding to a router prompt.
    fn ground_prompt(&self, mut prompt: String, definitions: &[ToolDefinition]) -> String {
        prompt.push_str(&format!(
            "\n## Environment\n\n- Working directory: {}\n- OS: {}\n",
            self.working_dir.display(),
            std::env::consts::OS
        ));
        if !definitions.is_empty() {
            let names: Vec<String> = definitions
                .iter()
                .map(|d| format!("- {}: {}", d.name, d.description))
                .collect();
            prompt.push_str(&format!(
                "\n## Tool use\n\nYou have these tools (function calls, rooted at the working directory above):\n{}\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool result` blocks — then answer the user.\n",
                names.join("\n")
            ));
        }
        prompt
    }

    /// Execute one round of model-requested tool calls.
    ///
    /// Failures become `ToolResult::Error` text so the model sees denials
    /// instead of stalling the loop.
    ///
    /// A round containing any mutating tool (`write_files` or
    /// `execute_commands` in its declared permissions) runs serially in
    /// caller order, so `[write_file(a), read_file(a)]` cannot race and
    /// the read is guaranteed to observe the write. All-read-only rounds
    /// still run concurrently — their results cannot depend on each other
    /// or on external state they did not observe themselves.
    async fn run_tool_calls(&self, calls: &[ToolCall]) -> ToolRound {
        let mut any_mutating = false;
        for call in calls {
            if let Some(perms) = self.tools.get_permissions(&call.tool_name).await
                && (perms.write_files || perms.execute_commands)
            {
                any_mutating = true;
                break;
            }
        }

        let raw_results: Vec<(Result<ToolResult>, u64)> = if any_mutating {
            let mut out = Vec::with_capacity(calls.len());
            for call in calls {
                let start = std::time::Instant::now();
                let res = self
                    .tools
                    .execute_tool(&call.tool_name, &call.arguments, &self.tool_context)
                    .await;
                out.push((res, start.elapsed().as_millis() as u64));
            }
            out
        } else {
            let futs: Vec<_> = calls
                .iter()
                .map(|call| {
                    let start = std::time::Instant::now();
                    async move {
                        let res = self
                            .tools
                            .execute_tool(&call.tool_name, &call.arguments, &self.tool_context)
                            .await;
                        (res, start.elapsed().as_millis() as u64)
                    }
                })
                .collect();
            futures::future::join_all(futs).await
        };
        let mut results = Vec::with_capacity(calls.len());
        let mut elapsed_ms = Vec::with_capacity(calls.len());
        let mut block = String::from("## Tool results\n");
        for (call, (res, ms)) in calls.iter().zip(raw_results) {
            elapsed_ms.push(ms);
            let result = match res {
                Ok(r) => r,
                Err(e) => ToolResult::Error(e.to_string()),
            };
            let rendered = match &result {
                ToolResult::Success(v) => v.to_string(),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };
            // Cap huge outputs (directory dumps) so context survives.
            let rendered = if rendered.len() > 4000 {
                format!(
                    "{}… [truncated {} chars]",
                    &rendered[..4000],
                    rendered.len() - 4000
                )
            } else {
                rendered
            };
            block.push_str(&format!(
                "\n### {} {}\n{}\n",
                call.tool_name, call.arguments, rendered
            ));
            results.push(result);
        }
        ToolRound {
            results,
            prompt_block: block,
            elapsed_ms,
        }
    }

    /// Run maintenance tasks
    pub async fn run_maintenance(&self) -> Result<()> {
        // Perform periodic maintenance
        // - Compact memory
        // - Clean up expired locks
        // - Update skill cache

        tracing::debug!("Running engine maintenance");
        Ok(())
    }

    /// Shutdown the engine
    pub async fn shutdown(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if !*running {
            return Ok(()); // Already stopped
        }

        *running = false;

        // Cleanup
        // - Stop all agents
        // - Release all locks
        // - Flush memory

        tracing::info!("KOD engine shutdown");
        Ok(())
    }

    /// Check if engine is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Get router reference
    pub fn router(&self) -> &TaskRouter {
        &self.router
    }

    /// Load skills from a directory into the router's matcher.
    pub async fn load_skills(&self, skills_dir: &std::path::Path) -> Result<usize> {
        self.router.load_skills(skills_dir).await
    }

    /// Load skills from every directory in `dirs`, skipping any that do
    /// not exist. Returns the total number of skill files read across all
    /// directories. Skills sharing a name across directories count once
    /// in the matcher (later dirs shadow earlier ones) but each file is
    /// counted here, so the returned number is "files loaded", not
    /// "distinct skills available" — use [`loaded_skill_names`] for the
    /// deduplicated set.
    pub async fn load_skills_from_dirs(&self, dirs: &[std::path::PathBuf]) -> Result<usize> {
        let mut total = 0;
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            match self.load_skills(dir).await {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %e,
                        "Could not load skills from directory"
                    );
                }
            }
        }
        Ok(total)
    }

    /// Names of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_names(&self) -> Vec<String> {
        self.router.loaded_skill_names().await
    }

    /// Names + descriptions of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_details(&self) -> Vec<(String, String)> {
        self.router.loaded_skill_details().await
    }

    /// Ask the running prompt to stop at the next round boundary.
    /// The TUI also aborts its background task, so the UI clears at once;
    /// this flag makes the engine side cooperate (goal loops, tool rounds).
    pub fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Clear a previous cancel (called when a new prompt is dispatched).
    pub fn clear_cancel(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// True if [`KodEngine::request_cancel`] was called and not cleared.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Queue a steering note while a prompt is running. It is injected
    /// into the conversation after the current tool round finishes, so the
    /// model course-corrects on the next round instead of starting over.
    pub async fn steer(&self, note: &str) {
        let note = note.trim();
        if !note.is_empty() {
            self.steer_queue.write().await.push(note.to_string());
        }
    }

    /// Drain queued steer notes (each is applied once, in order).
    async fn take_steers(&self) -> Vec<String> {
        std::mem::take(&mut *self.steer_queue.write().await)
    }

    /// Remember one turn, truncating long texts and keeping only the most
    /// recent [`MAX_HISTORY_TURNS`] turns.
    async fn record_turn(&self, user: bool, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let short = if text.len() > MAX_TURN_CHARS {
            format!("{}… [truncated]", &text[..MAX_TURN_CHARS])
        } else {
            text.to_string()
        };
        let mut history = self.history.write().await;
        history.push(HistoryTurn { user, text: short });
        let excess = history.len().saturating_sub(MAX_HISTORY_TURNS);
        if excess > 0 {
            history.drain(..excess);
        }
    }

    /// Render past turns oldest-first for the prompt, newest-first dropped
    /// once over [`MAX_HISTORY_CHARS`]. Empty before the first turn.
    async fn render_history(&self) -> String {
        let history = self.history.read().await;
        if history.is_empty() {
            return "(start of conversation)".to_string();
        }
        let mut out = String::new();
        for turn in history.iter().rev() {
            let line = format!(
                "{}: {}\n",
                if turn.user { "User" } else { "Assistant" },
                turn.text
            );
            if out.len() + line.len() > MAX_HISTORY_CHARS {
                break;
            }
            out.insert_str(0, &line);
        }
        out
    }

    /// Forget the transcript (`/clear`). Display messages are cleared
    /// separately by the TUI — this is the model's copy.
    pub async fn clear_history(&self) {
        self.history.write().await.clear();
    }

    /// Keep only the last `max_turns` turns (`/compact`). Used by the TUI
    /// so the model window stays bounded without wiping history entirely.
    pub async fn compact_history(&self, max_turns: usize) {
        let mut history = self.history.write().await;
        if history.len() > max_turns {
            let drop = history.len() - max_turns;
            history.drain(..drop);
        }
    }
}

/// Assemble one [`ToolCall`] from streamed `ToolCallStart`/`ToolCallDelta`
/// framing. Deltas arrive as JSON text; unparseable fragments are kept as a
/// raw string so the call still executes instead of being dropped.
fn finish_stream_call(name: String, args: &str) -> ToolCall {
    let arguments: serde_json::Value =
        serde_json::from_str(args).unwrap_or(serde_json::Value::String(args.to_string()));
    ToolCall {
        tool_name: name,
        arguments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_engine_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let engine = KodEngine::new(RouterConfig::default(), db_path).unwrap();

        // Engine starts not running
        assert!(!engine.is_running().await);

        // Start engine
        engine.start().await.unwrap();
        assert!(engine.is_running().await);

        // Shutdown
        engine.shutdown().await.unwrap();
        assert!(!engine.is_running().await);
    }

    #[test]
    fn test_tool_done_marker_roundtrip() {
        let header = "execute_command command=cargo test";
        let summary = "line1\nline2\nline3";
        let chunk = tool_done_marker(header, summary, 1340);
        let (h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!(h, header);
        assert_eq!(s, summary);
        assert_eq!(ms, 1340);
    }

    #[test]
    fn test_tool_done_marker_sanitizes_nul() {
        let chunk = tool_done_marker("a\0b", "c\0d", 7);
        let (h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!((h, s, ms), ("a b", "c d", 7));
    }

    #[test]
    fn test_tool_done_marker_rejects_other_chunks() {
        assert!(parse_tool_done("plain text").is_none());
        assert!(parse_tool_done(&tool_start_marker("read_file")).is_none());
        // Malformed duration degrades to 0 instead of dropping the row.
        let raw = format!("{TOOL_DONE_MARKER}h\0s\0abc");
        assert_eq!(parse_tool_done(&raw), Some(("h", "s", 0)));
        // Truncated payload is not a completion.
        let raw = format!("{TOOL_DONE_MARKER}only-header");
        assert!(parse_tool_done(&raw).is_none());
    }

    #[test]
    fn test_format_duration_ms() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(999), "999ms");
        assert_eq!(format_duration_ms(1000), "1.0s");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(65_000), "1m05s");
    }

    #[test]
    fn test_summarize_tool_result_shapes() {
        let err = summarize_tool_result("read_file", &ToolResult::Error("boom".to_string()));
        assert!(err.starts_with("Error:"), "got: {err}");
        let ok = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({"stdout": "hi\n", "stderr": ""})),
        );
        assert_eq!(ok, "hi");
        let hdr = format_tool_header("read_file", &serde_json::json!({"path": "/a/b/c/main.rs"}));
        assert!(hdr.starts_with("read_file path="), "got: {hdr}");
        // read_file success stays compact: path + size + preview, not a dump.
        let read = summarize_tool_result(
            "read_file",
            &ToolResult::Success(
                serde_json::json!({"path": "/a/main.rs", "content": "one\ntwo\nthree\nfour\n"}),
            ),
        );
        assert!(read.contains("4 lines"), "got: {read}");
        assert!(read.contains("one\ntwo\nthree"), "got: {read}");
        assert!(!read.contains("four"), "got: {read}");
    }

    /// A round containing a mutating tool must run serially in caller
    /// order: the read must observe the write that precedes it in the
    /// same round. Before the serialization fix, join_all could run the
    /// read before the write committed, and this test would flake (or
    /// fail when the file did not exist yet).
    #[tokio::test]
    async fn test_run_tool_calls_serializes_mutating_round() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");

        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![
            ToolCall {
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "serialize_probe.txt",
                    "content": "hello-serial"
                }),
            },
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "serialize_probe.txt" }),
            },
        ];

        let round = engine.run_tool_calls(&calls).await;
        assert_eq!(round.results.len(), 2);

        // Write must succeed.
        match &round.results[0] {
            ToolResult::Success(_) => {}
            other => panic!("write_file did not succeed: {:?}", other),
        }
        // Read must observe the write.
        match &round.results[1] {
            ToolResult::Success(v) => {
                assert_eq!(
                    v["content"], "hello-serial",
                    "read did not observe write — round raced: {:?}",
                    v
                );
            }
            other => panic!("read_file did not succeed: {:?}", other),
        }
    }

    /// An all-read-only round is safe to parallelize; this test just
    /// verifies both results come back, not the execution order.
    #[tokio::test]
    async fn test_run_tool_calls_parallelizes_read_only_round() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "AAA").unwrap();
        std::fs::write(temp.path().join("b.txt"), "BBB").unwrap();

        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            },
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "b.txt" }),
            },
        ];
        let round = engine.run_tool_calls(&calls).await;
        assert_eq!(round.results.len(), 2);
        assert_eq!(round.elapsed_ms.len(), 2);
        // Order matches caller order regardless of scheduling.
        match (&round.results[0], &round.results[1]) {
            (ToolResult::Success(a), ToolResult::Success(b)) => {
                assert_eq!(a["content"], "AAA");
                assert_eq!(b["content"], "BBB");
            }
            other => panic!("expected two successes, got {:?}", other),
        }
    }
}
