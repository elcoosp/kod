//! Session log: a JSONL record of every tool call the engine makes.
//!
//! Two uses, both worth the file:
//!
//! 1. **Debugging.** "What actually happened in that run?" is a
//!    question the terminal transcript cannot answer once the tool
//!    rows have scrolled past. A JSONL file can be grepped, diffed,
//!    and — most usefully — replayed.
//! 2. **Regression suite.** `kod replay <log>` re-executes the tool
//!    calls without the model. The same tool sequence against a new
//!    commit of the codebase is a concrete check that the tools still
//!    work; a diff of the results is a concrete check that the
//!    codebase still behaves the same way.
//!
//! The log is append-only and opened with `O_APPEND`; concurrent
//! writers append without an advisory lock. A single line is small
//! enough that the kernel's atomic-append guarantee on a regular file
//! is sufficient — the same reasoning `persist_history_entry` uses in
//! the TUI.

use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One recorded event. Tagged by `kind` so a future variant can be
/// added without a schema migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    /// A tool call and its result.
    ToolCall {
        /// Unix milliseconds when the call started.
        timestamp_ms: u64,
        /// The engine-transcript key the call ran under. The
        /// interactive session uses the empty string
        /// (`DEFAULT_TRANSCRIPT_KEY` in `kod-core`'s engine); a swarm
        /// agent uses `swarm:<agent-id>` so concurrent agents do not
        /// interleave their turns.
        holder: String,
        tool_name: String,
        arguments: serde_json::Value,
        /// Wall time the call took, in milliseconds.
        duration_ms: u64,
        /// The result, in the same shape the model saw. Success carries
        /// the raw `Success(Value)` payload under `success`, an
        /// `Error(String)` under `error`, and a
        /// `RequiresConfirmation` under `requires_confirmation`.
        result: serde_json::Value,
    },
    /// The routing layer gave up on one endpoint and moved to the next
    /// in the chain (AD-15). Written once per transition, so a session
    /// log carries a clean audit trail of which endpoint served which
    /// turn.
    ModelFallback {
        timestamp_ms: u64,
        /// The transcript key the fallback happened on ("session",
        /// "swarm:agent-3", ...).
        holder: String,
        from: String,
        to: String,
        error: String,
    },
    /// Cost of one provider call, in USD. Written after every
    /// completion that reported usage and an endpoint with
    /// `pricing` configured. Endpoints without pricing log nothing
    /// here — the field is not a guess.
    Cost {
        timestamp_ms: u64,
        holder: String,
        endpoint: String,
        model: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        cost_usd: f64,
    },
    /// A policy decision made before a tool call (D3, AD-15).
    /// One entry per tool call, so the JSONL carries an audit trail
    /// of every allow/deny/ask the engine answered.
    PolicyDecision {
        timestamp_ms: u64,
        holder: String,
        tool_name: String,
        /// "allow" | "deny" | "ask"
        outcome: String,
        /// Human-readable description of the rule that fired.
        rule: String,
        /// Which layer produced the decision
        /// ("preset" | "global-config" | "project-policy" |
        ///  "cli-override" | "session-deny").
        source: String,
    },
    /// A long-term memory entry was written (D2.4, AD-15).
    ///
    /// Emitted by each of the three write channels: extraction at
    /// end of session, the `memory_save` tool, and the user's
    /// `/remember` command. `channel` names which one; the value is
    /// one of `"extraction" | "tool" | "user"`.
    MemoryWrite {
        timestamp_ms: u64,
        memory_id: String,
        channel: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    /// An approval decision was made on a pending tool call
    /// (D3.2, AD-15). `decision` is `"approve" | "deny" |
    /// "deny-always" | "timeout"`.
    Approval {
        timestamp_ms: u64,
        holder: String,
        tool_name: String,
        decision: String,
        /// Tier 2.3 — when the caller substituted arguments (an
        /// `ApproveWith` decision), the new arguments are recorded
        /// here so `/log` shows what changed and by how much. `None`
        /// for every plain approve/deny.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edit: Option<serde_json::Value>,
    },
    /// A post-write LSP diagnostics pass ran on a file (D5.2,
    /// AD-15). One entry per file per pass. The counts let a reader
    /// see whether a write introduced or resolved errors without
    /// scanning the full diagnostic list.
    Diagnostics {
        timestamp_ms: u64,
        file: String,
        error_count: usize,
        warning_count: usize,
    },
    /// One memory retrieval event (Tier 2.4). Logged per prompt so
    /// `/memory eval` can compute hit rates and identify queries
    /// that repeatedly miss.
    MemoryRetrieval {
        timestamp_ms: u64,
        /// Monotonic turn id that triggered the retrieval.
        turn_id: u64,
        /// FNV-1a hash of the query text.
        query_hash: String,
        /// Retrieved entry ids with their relevance scores, in the
        /// order they were selected.
        retrieved: Vec<(String, f32)>,
        /// Entries the reply actually referenced, as classified by
        /// Jev. Empty when the classifier did not run.
        #[serde(default)]
        referenced: Vec<String>,
        /// True when the next user message looked like a correction
        /// of this turn. Filled in on the *next* turn.
        #[serde(default)]
        user_corrected: bool,
        /// How many entries retrieval considered before filtering.
        /// `retrieved.len()` is what survived; the difference is what
        /// the filters dropped, and `dropped` says why.
        #[serde(default)]
        considered: usize,
        /// `(id, reason)` for entries that were retrieved and then
        /// dropped before injection. The reasons are the harness's
        /// words: `"jev: irrelevant"`, `"injected 12m ago"`.
        #[serde(default)]
        dropped: Vec<(String, String)>,
    },
    /// One redaction event, aggregated per rule per write (Tier 1.3).
    /// Emitted *after* the entry whose payload was redacted, so a
    /// reader that wants the raw trail sees what fired where.
    Redaction {
        timestamp_ms: u64,
        /// The rule names that fired, with their counts. Aggregated
        /// over the whole entry so a tool result with three OpenAI
        /// keys produces one `openai-key: 3` item.
        rules: Vec<kod_types::redact::Redaction>,
    },
    /// Semantic classification of a tool call's outcome (P5.3).
    /// Written only when the Jev integration is enabled and the call
    /// ran for at least [`MIN_JEV_CLASSIFY_MS`] — a sub-100ms call is
    /// not worth a network round-trip. Distinct from `ToolCall`
    /// (which is syntactic: name, args, duration, raw result) so a
    /// reader that wants the raw trail is not slowed by the extra
    /// entries.
    ToolOutcome {
        timestamp_ms: u64,
        holder: String,
        tool_name: String,
        /// One of `success` | `partial` | `failure` | `irrelevant`.
        outcome: String,
        /// One of `none` | `minor` | `significant` | `critical`.
        user_visible_impact: String,
        /// Confidence in `[0, 1]`; the maximum of the two answer
        /// probabilities below.
        confidence: f32,
        /// Wall time for the classification call, ms.
        latency_ms: u64,
        /// `"jev"` | `"heuristic"`.
        source: String,
    },
    /// One Jev (TypeSafe System One) decision. Written by the
    /// `JevClient` wrapper for every call site — including the
    /// heuristic fallback when Jev is disabled or errors. The
    /// `source` field distinguishes the three cases so
    /// `/jev stats` can report Jev's share without re-parsing
    /// the purpose string.
    JevDecision {
        timestamp_ms: u64,
        /// The engine-transcript key the decision served.
        holder: String,
        /// A short label for the call site (`tool_filter`,
        /// `task_classify`, `auto_approve`, ...). Not sent to
        /// TypeSafe.
        purpose: String,
        /// First 200 characters of the state sent to Jev.
        state_preview: String,
        /// Summary of the question(s) — for a multi-question
        /// call, a comma-separated list of keys.
        questions_summary: String,
        /// The typed answers, in a shape the call site chose.
        answers: serde_json::Value,
        /// The winning answer's confidence in [0, 1].
        confidence: f32,
        /// Wall time for the Jev call, in milliseconds.
        latency_ms: u64,
        /// True when the answer came from the decision cache.
        cached: bool,
        /// "jev" | "heuristic" | "llm".
        source: String,
    },
}

/// Append-only writer for a session log.
pub struct SessionRecorder {
    path: PathBuf,
    writer: Mutex<std::fs::File>,
    /// Applied to every entry's payload before serialisation. The
    /// default uses the built-in rule set; `with_redactor` swaps it.
    /// The field is never `None` — a caller that wants no redaction
    /// passes a `Redactor::with_rules(vec![])` and pays a small
    /// hashmap lookup per line.
    redactor: kod_types::redact::Redactor,
}

impl SessionRecorder {
    /// Open `path` for append, creating it and any missing parent
    /// directories. An existing file is appended to, never truncated —
    /// a `kod replay` run can then be handed a file that has been
    /// extended across multiple sessions.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(KodError::Io)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(KodError::Io)?;
        Ok(Self {
            path,
            writer: Mutex::new(file),
            redactor: kod_types::redact::Redactor::default(),
        })
    }

    /// Replace the redactor. A caller that wants a stricter or
    /// looser rule set builds one and installs it here; the field is
    /// used for every subsequent `record`.
    pub fn with_redactor(mut self, redactor: kod_types::redact::Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    /// Append one entry. Flushes after every write: a crash mid-session
    /// should leave a complete log up to the last call, not a buffered
    /// prefix that `kod replay` cannot parse.
    ///
    /// The entry's payload is redacted in place before serialisation
    /// (Tier 1.3). Redaction counts are appended as one
    /// `SessionEntry::Redaction` line so `/stats` can report them
    /// without a second scan.
    pub fn record(&self, entry: &SessionEntry) -> Result<()> {
        // Clone so we can mutate for redaction without changing the
        // caller's entry.
        let mut cloned = entry.clone();
        let mut redactions: Vec<kod_types::redact::Redaction> = Vec::new();
        match &mut cloned {
            SessionEntry::ToolCall {
                arguments, result, ..
            } => {
                redactions.extend(self.redactor.redact_json(arguments));
                redactions.extend(self.redactor.redact_json(result));
            }
            SessionEntry::PolicyDecision { rule, .. } => {
                let (r, ev) = self.redactor.redact(rule);
                if !ev.is_empty() {
                    *rule = r;
                    redactions.extend(ev);
                }
            }
            SessionEntry::ModelFallback { error, .. } => {
                let (r, ev) = self.redactor.redact(error);
                if !ev.is_empty() {
                    *error = r;
                    redactions.extend(ev);
                }
            }
            SessionEntry::JevDecision {
                state_preview,
                answers,
                ..
            } => {
                let (r, ev) = self.redactor.redact(state_preview);
                if !ev.is_empty() {
                    *state_preview = r;
                    redactions.extend(ev);
                }
                redactions.extend(self.redactor.redact_json(answers));
            }
            _ => {}
        }
        let line =
            serde_json::to_string(&cloned).map_err(|e| KodError::Serialization(e.to_string()))?;
        {
            let mut w = self.writer.lock().unwrap();
            // H-D10: one `write_all` of the full line + newline, not
            // `writeln!`. `writeln!` on a raw File issues two
            // `write` syscalls (payload, newline); O_APPEND atomicity
            // is per-syscall, so two recorders on the same path could
            // interleave fragments and corrupt a line. Building the
            // buffer once and issuing a single write closes that.
            let mut buf = Vec::with_capacity(line.len() + 1);
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
            w.write_all(&buf).map_err(KodError::Io)?;
            if !redactions.is_empty() {
                // Aggregate by rule for a compact line.
                use std::collections::BTreeMap;
                let mut agg: BTreeMap<String, usize> = BTreeMap::new();
                for r in redactions {
                    *agg.entry(r.rule).or_insert(0) += r.count;
                }
                let rules: Vec<kod_types::redact::Redaction> = agg
                    .into_iter()
                    .map(|(rule, count)| kod_types::redact::Redaction { rule, count })
                    .collect();
                let entry = SessionEntry::Redaction {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    rules,
                };
                let line = serde_json::to_string(&entry)
                    .map_err(|e| KodError::Serialization(e.to_string()))?;
                let mut buf = Vec::with_capacity(line.len() + 1);
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                w.write_all(&buf).map_err(KodError::Io)?;
            }
            w.flush().map_err(KodError::Io)?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush any buffered bytes. `record()` already flushes per line, so
    /// this is a belt-and-braces call from `shutdown()`: it is a no-op
    /// when there is nothing pending, and it makes the shutdown contract
    /// explicit ("everything written by this recorder is on disk when
    /// this returns") without relying on the per-line flush not
    /// regressing later.
    pub fn flush(&self) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.flush().map_err(KodError::Io)?;
        Ok(())
    }
}

/// Read every entry in `path`, in file order. A malformed line is a
/// hard error — the file is machine-written, and a partial line means
/// the log is corrupt, not that the reader should skip.
pub fn read_session(path: &Path) -> Result<Vec<SessionEntry>> {
    let raw = std::fs::read_to_string(path).map_err(KodError::Io)?;
    let mut out = Vec::new();
    // H-D10: a crash-truncated final line (no trailing newline) is the
    // exact case per-line flushing exists to survive. Treat it as
    // "the last write was interrupted, the log up to the previous
    // newline is intact" — drop the partial line with a warning
    // rather than failing the whole read.
    let ends_with_newline = raw.ends_with('\n');
    let lines: Vec<&str> = raw.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let is_last = i + 1 == lines.len();
        let last_is_partial = is_last && !ends_with_newline;
        match serde_json::from_str::<SessionEntry>(line) {
            Ok(e) => out.push(e),
            Err(e) => {
                if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                    tracing::warn!(
                        line = i + 1,
                        "session log line has an unknown kind; \
                         skipping (forward-compat)"
                    );
                    continue;
                }
                if last_is_partial {
                    tracing::warn!(
                        line = i + 1,
                        "session log tail is a partial line (no trailing \
                         newline); dropping it and keeping the rest",
                    );
                    continue;
                }
                return Err(KodError::Deserialization(format!("line {}: {}", i + 1, e)));
            }
        }
    }
    Ok(out)
}

/// Default session-log path for a fresh session:
/// `~/.kod/sessions/<unix-ms>.jsonl`. A caller that wants a stable
/// path (a test, a script that names its own log) sets
/// `KOD_SESSION_LOG` and this is not consulted.
pub fn default_session_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some(
        home.join(".kod")
            .join("sessions")
            .join(format!("{ts}.jsonl")),
    )
}


/// Which shape a rehydrated transcript takes.
///
/// Both forms come from the same JSONL entries; they differ in what
/// the caller can do with the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RehydrationMode {
    /// One `User`-role prose message per tool call. This is what the
    /// text-protocol `render_history_for` can display — it skips
    /// `Tool`-role rows, so a structured pair would be invisible.
    #[default]
    Prose,
    /// An `Assistant` message carrying `tool_calls`, then a `Tool`
    /// message carrying each result, linked by `tool_call_id`. This
    /// is the shape a `CompletionRequest` puts on the wire; it is
    /// not what the current text renderer shows, so a caller that
    /// wants the resumed conversation to *affect the prompt* wants
    /// `Prose`.
    Structured,
}

/// Rebuild a transcript from the tool-call entries a session log
/// recorded for one holder.
///
/// This is the P2 "rehydrate on restart" half of the context engine:
/// `read_session` recovers the JSONL entries, and this function turns
/// the `ToolCall` entries back into the `ChatMessage` pairs the model
/// saw — an assistant turn carrying the call, immediately followed by
/// a tool turn carrying its result. Other entry kinds (Cost,
/// PolicyDecision, …) are audit records, not transcript; they are
/// skipped.
///
/// The function is pure and deterministic: given the same entries
/// and holder it produces the same messages, in log order, with
/// stable `tool_call_id` linkage. Timestamps come from the entry's
/// `timestamp_ms`; a value that cannot be represented as an
/// `OffsetDateTime` falls back to the Unix epoch rather than
/// dropping the turn.
pub fn rehydrate_turns(
    entries: &[SessionEntry],
    holder: &str,
) -> Vec<kod_types::ChatMessage> {
    use kod_types::{ChatMessage, MessageId, MessageRole};

    let mut out: Vec<ChatMessage> = Vec::new();
    for entry in entries {
        let SessionEntry::ToolCall {
            timestamp_ms,
            holder: entry_holder,
            tool_name,
            arguments,
            result,
            ..
        } = entry
        else {
            continue;
        };
        if entry_holder != holder {
            continue;
        }

        let ts = time::OffsetDateTime::from_unix_timestamp_nanos(
            (*timestamp_ms as i128) * 1_000_000,
        )
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);

        // A stable call id ties the assistant tool-call message to
        // the tool-result message that answers it. Log entries do not
        // carry the provider's original id, so we synthesize one from
        // the position in the rebuilt transcript.
        let call_id = format!("rehydrated-{:04}", out.len());

        let call = kod_types::ToolCall {
            id: Some(call_id.clone()),
            tool_name: tool_name.clone(),
            arguments: arguments.clone(),
        };

        let mut assistant = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            String::new(), // content lives in tool_calls
            ts,
        );
        assistant.tool_calls.push(call);
        out.push(assistant);

        let content = render_result_text(result);
        let mut tool_msg = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            content,
            ts,
        );
        tool_msg.tool_call_id = Some(call_id);
        out.push(tool_msg);
    }
    out
}

/// Render a `SessionEntry::ToolCall.result` JSON value into the text
/// a tool-role message carries. The engine writes one of
/// `{"success": <v>}`, `{"error": "<s>"}`, or
/// `{"requires_confirmation": {...}}`; anything else falls through
/// as compact JSON.
fn render_result_text(result: &serde_json::Value) -> String {
    if let Some(v) = result.get("success") {
        return match v {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
    }
    if let Some(s) = result.get("error").and_then(|v| v.as_str()) {
        return format!("Error: {s}");
    }
    if result.get("requires_confirmation").is_some() {
        return "(rehydrated: tool call required confirmation)".to_string();
    }
    serde_json::to_string(result).unwrap_or_default()
}


/// Rebuild a transcript from a session log as **prose** messages.
///
/// The structured form returned by [`rehydrate_turns`] is the right
/// shape for a future `CompletionRequest` path (AD-01) where tool
/// messages go on the wire. The engine's current text-protocol
/// `render_history` deliberately skips Tool-role rows — they reach
/// the model via the `## Tool results` block on the live path — so a
/// rehydrated turn must be prose to survive that filter and appear
/// in the rendered prompt.
///
/// Each recorded `ToolCall` entry becomes one User-role message
/// summarising the call and its result. Pure and deterministic.
pub fn rehydrate_prose_turns(
    entries: &[SessionEntry],
    holder: &str,
) -> Vec<kod_types::ChatMessage> {
    use kod_types::{ChatMessage, MessageId, MessageRole};

    let mut out: Vec<ChatMessage> = Vec::new();
    for entry in entries {
        let SessionEntry::ToolCall {
            timestamp_ms,
            holder: entry_holder,
            tool_name,
            arguments,
            result,
            ..
        } = entry
        else {
            continue;
        };
        if entry_holder != holder {
            continue;
        }

        let ts = time::OffsetDateTime::from_unix_timestamp_nanos(
            (*timestamp_ms as i128) * 1_000_000,
        )
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);

        let args = serde_json::to_string(arguments).unwrap_or_default();
        let body = format!(
            "[rehydrated] Tool call `{tool_name}` with arguments {args} returned:\n{}",
            render_result_text(result),
        );
        out.push(ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            body,
            ts,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry() -> SessionEntry {
        SessionEntry::ToolCall {
            timestamp_ms: 1_700_000_000_000,
            holder: "session".to_string(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
            duration_ms: 7,
            result: serde_json::json!({"success": {"path": "src/main.rs", "content": "fn main() {}"}}),
        }
    }

    #[test]
    fn record_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        let recorder = SessionRecorder::open(path.clone()).unwrap();
        recorder.record(&sample_entry()).unwrap();
        recorder.record(&sample_entry()).unwrap();

        let entries = read_session(&path).unwrap();
        assert_eq!(entries.len(), 2);
        match &entries[0] {
            SessionEntry::ToolCall {
                tool_name, holder, ..
            } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(holder, "session");
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }

    #[test]
    fn empty_file_reads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(read_session(&path).unwrap().is_empty());
    }

    #[test]
    fn malformed_line_is_an_error_naming_the_line() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.jsonl");
        std::fs::write(&path, "{\"kind\":\"tool_call\"\n").unwrap();
        let err = read_session(&path).unwrap_err();
        assert!(err.to_string().contains("line 1"), "got: {err}");
    }

    #[test]
    fn unknown_kind_line_is_skipped_not_fatal() {
        // A future version of kod could add a new SessionEntry variant.
        // An older build reading that file must not fail the whole
        // read; it skips the unknown line with a warning and keeps the
        // entries it understands. Regression guard for the
        // forward-compat path in `read_session`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("mixed.jsonl");
        std::fs::write(
            &path,
            concat!(
                // A valid entry the current build understands.
                "{\"kind\":\"tool_call\",\"timestamp_ms\":1,",
                "\"holder\":\"s\",\"tool_name\":\"read_file\",",
                "\"arguments\":{},\"duration_ms\":1,",
                "\"result\":{\"success\":{}}}\n",
                // A valid JSON line with an unknown `kind`.
                "{\"kind\":\"future_thing\",\"payload\":42}\n",
                // Another valid entry.
                "{\"kind\":\"tool_call\",\"timestamp_ms\":2,",
                "\"holder\":\"s\",\"tool_name\":\"write_file\",",
                "\"arguments\":{},\"duration_ms\":2,",
                "\"result\":{\"success\":{}}}\n"
            ),
        )
        .unwrap();

        let entries = read_session(&path).expect("forward-compat read must succeed");
        assert_eq!(
            entries.len(),
            2,
            "the unknown-kind line must be skipped, not surfaced",
        );
        match &entries[0] {
            SessionEntry::ToolCall { tool_name, .. } => {
                assert_eq!(tool_name, "read_file");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        match &entries[1] {
            SessionEntry::ToolCall { tool_name, .. } => {
                assert_eq!(tool_name, "write_file");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn open_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("session.jsonl");
        let recorder = SessionRecorder::open(path.clone()).unwrap();
        recorder.record(&sample_entry()).unwrap();
        assert!(path.exists());
    }
}

#[cfg(test)]
mod coverage_entry_roundtrip {
    //! Every `SessionEntry` variant must round-trip through JSON so
    //! `kod replay` and `/log` can read a session written by any
    //! build. The existing tests cover `ToolCall`; the other six
    //! variants are the ones a new field silently breaks, because
    //! nothing writes them in the in-tree tests.
    use super::SessionEntry;

    fn tool_call() -> SessionEntry {
        SessionEntry::ToolCall {
            timestamp_ms: 1,
            holder: "session".into(),
            tool_name: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
            duration_ms: 7,
            result: serde_json::json!({"success": {}}),
        }
    }

    fn every_variant() -> Vec<SessionEntry> {
        vec![
            tool_call(),
            SessionEntry::ModelFallback {
                timestamp_ms: 2,
                holder: "s".into(),
                from: "a/1".into(),
                to: "b/2".into(),
                error: "connection reset".into(),
            },
            SessionEntry::Cost {
                timestamp_ms: 3,
                holder: "s".into(),
                endpoint: "local".into(),
                model: "qwen".into(),
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_usd: 0.001,
            },
            SessionEntry::PolicyDecision {
                timestamp_ms: 4,
                holder: "s".into(),
                tool_name: "write_file".into(),
                outcome: "allow".into(),
                rule: "preset Standard applies".into(),
                source: "preset".into(),
            },
            SessionEntry::MemoryWrite {
                timestamp_ms: 5,
                memory_id: "abc".into(),
                channel: "tool".into(),
                tags: vec!["auto-fact".into()],
            },
            SessionEntry::Approval {
                timestamp_ms: 6,
                holder: "s".into(),
                tool_name: "write_file".into(),
                decision: "approve".into(),
                edit: None,
            },
            SessionEntry::Diagnostics {
                timestamp_ms: 7,
                file: "a.rs".into(),
                error_count: 2,
                warning_count: 3,
            },
            SessionEntry::MemoryRetrieval {
                timestamp_ms: 11,
                turn_id: 42,
                query_hash: "deadbeef".into(),
                retrieved: vec![("mem-1".into(), 0.9), ("mem-2".into(), 0.4)],
                referenced: vec!["mem-1".into()],
                user_corrected: false,
                considered: 3,
                dropped: vec![("mem-3".into(), "injected 12m ago".into())],
            },
            SessionEntry::Redaction {
                timestamp_ms: 10,
                rules: vec![kod_types::redact::Redaction {
                    rule: "openai-key".to_string(),
                    count: 2,
                }],
            },
            SessionEntry::ToolOutcome {
                timestamp_ms: 9,
                holder: "s".into(),
                tool_name: "read_file".into(),
                outcome: "success".into(),
                user_visible_impact: "significant".into(),
                confidence: 0.92,
                latency_ms: 180,
                source: "jev".into(),
            },
            SessionEntry::JevDecision {
                timestamp_ms: 8,
                holder: "s".into(),
                purpose: "tool_filter".into(),
                state_preview: "User request: run the tests".into(),
                questions_summary: "filesystem,shell".into(),
                answers: serde_json::json!({"filesystem": 0.92, "shell": 0.31}),
                confidence: 0.92,
                latency_ms: 210,
                cached: false,
                source: "jev".into(),
            },
        ]
    }

    #[test]
    fn every_variant_round_trips_through_json() {
        for e in &every_variant() {
            let json = serde_json::to_string(e).expect("serialize");
            let parsed: SessionEntry = serde_json::from_str(&json).expect("parse");
            let re = serde_json::to_string(&parsed).expect("re-serialize");
            assert_eq!(json, re, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn kind_tag_is_the_variant_name_in_snake_case() {
        // The on-disk schema is `<variant>` -> snake_case via the
        // container attribute. A caller that branches on the tag
        // (an external log viewer, a future `kod log`) depends on
        // this naming, so a rename is a schema break.
        let cases = [
            (tool_call(), "tool_call"),
            (
                SessionEntry::Cost {
                    timestamp_ms: 0,
                    holder: "h".into(),
                    endpoint: "e".into(),
                    model: "m".into(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cost_usd: 0.0,
                },
                "cost",
            ),
            (
                SessionEntry::Approval {
                    timestamp_ms: 0,
                    holder: "h".into(),
                    tool_name: "t".into(),
                    decision: "deny".into(),
                    edit: None,
                },
                "approval",
            ),
        ];
        for (entry, expected_kind) in cases {
            let v: serde_json::Value = serde_json::to_value(&entry).unwrap();
            assert_eq!(v["kind"], expected_kind, "for {entry:?}");
        }
    }

    #[test]
    fn malformed_line_is_rejected_but_forward_compat_is_tolerated() {
        // Two separate behaviours the reader must keep distinct:
        // unknown `kind` is skipped (forward compat), invalid JSON
        // is a hard error (the log is corrupt).
        use super::read_session;
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mixed.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"kind\":\"tool_call\",\"timestamp_ms\":1,\"holder\":\"s\",",
                "\"tool_name\":\"r\",\"arguments\":{},\"duration_ms\":1,",
                "\"result\":{\"success\":{}}}\n",
                "not json at all\n"
            ),
        )
        .unwrap();
        let err = read_session(&path).unwrap_err();
        assert!(err.to_string().contains("line 2"), "got: {err}");
    }
}

#[cfg(test)]
mod coverage_session_paths {
    //! `default_session_path` is the one place the log location is
    //! decided. A regression that dropped the timestamp or the
    //! directory would make `kod replay` point at the wrong file
    //! or overwrite a previous session's log.
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_session_path_is_under_kod_sessions() {
        // The default lives under `<home>/.kod/sessions/`. Whether
        // a home directory is available depends on the test
        // environment; when it is available, the path has the
        // expected suffix and a `.jsonl` extension.
        if let Some(p) = default_session_path() {
            let s = p.to_string_lossy();
            assert!(s.contains(".kod"), "not under .kod: {s}");
            assert!(s.contains("sessions"), "not under sessions: {s}");
            assert!(s.ends_with(".jsonl"), "wrong extension: {s}");
        }
    }

    #[test]
    fn two_default_paths_differ_within_the_same_process() {
        // The path includes a millisecond timestamp. Two calls in
        // the same process are microseconds apart, so they may or
        // may not differ — but they must both exist and be
        // well-formed. The test records the contract rather than
        // asserting non-determinism.
        if let (Some(a), Some(b)) = (default_session_path(), default_session_path()) {
            assert!(a.extension().and_then(|s| s.to_str()) == Some("jsonl"));
            assert!(b.extension().and_then(|s| s.to_str()) == Some("jsonl"));
        }
    }

    #[test]
    fn open_records_and_reads_from_a_path_under_a_nested_directory() {
        // A path whose parent does not exist must be created by
        // `open`. Regression target: a log directory that is
        // removed between sessions.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("s.jsonl");
        let rec = SessionRecorder::open(nested.clone()).unwrap();
        let entry = SessionEntry::Approval {
            timestamp_ms: 1,
            holder: "h".into(),
            tool_name: "write_file".into(),
            decision: "approve".into(),
            edit: None,
        };
        rec.record(&entry).unwrap();
        let read = read_session(&nested).unwrap();
        assert_eq!(read.len(), 1);
        match &read[0] {
            SessionEntry::Approval { tool_name, .. } => {
                assert_eq!(tool_name, "write_file");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn recorder_path_accessor_reports_the_open_path() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.jsonl");
        let rec = SessionRecorder::open(p.clone()).unwrap();
        // The on-disk path is exactly what was passed in.
        assert_eq!(rec.path(), p.as_path());
    }

    #[test]
    fn flush_on_an_empty_recorder_is_ok() {
        let tmp = TempDir::new().unwrap();
        let rec = SessionRecorder::open(tmp.path().join("e.jsonl")).unwrap();
        rec.flush().unwrap();
    }

    #[test]
    fn multiple_recorders_append_to_the_same_path() {
        // `open` uses `OpenOptions::new().append(true)`; two
        // recorders on the same path must not truncate each other.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("multi.jsonl");
        let a = SessionRecorder::open(p.clone()).unwrap();
        a.record(&SessionEntry::Approval {
            timestamp_ms: 1,
            holder: "a".into(),
            tool_name: "t".into(),
            decision: "approve".into(),
            edit: None,
        })
        .unwrap();
        let b = SessionRecorder::open(p.clone()).unwrap();
        b.record(&SessionEntry::Approval {
            timestamp_ms: 2,
            holder: "b".into(),
            tool_name: "t".into(),
            decision: "deny".into(),
            edit: None,
        })
        .unwrap();
        let read = read_session(&p).unwrap();
        assert_eq!(read.len(), 2);
    }
}

#[cfg(test)]
mod coverage_truncated_tail {
    //! H-D10 regression. A session log whose last write was
    //! interrupted (SIGKILL mid-`write_all`, disk full, power loss)
    //! ends without a trailing newline. Reading it must succeed and
    //! return every complete line, not refuse the whole file.
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn truncated_tail_is_dropped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("s.jsonl");
        // One full, valid line followed by a partial one.
        let full = "{\"kind\":\"approval\",\"timestamp_ms\":1,\"holder\":\"h\",\"tool_name\":\"t\",\"decision\":\"approve\",\"edit\":null}\n";
        let partial = "{\"kind\":\"approval\",\"timestamp_ms\":2,\"holder\":\"h\",\"tool";
        std::fs::write(&p, format!("{full}{partial}")).unwrap();
        let entries = read_session(&p).expect("partial tail must not fail the read");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn a_corrupt_middle_line_is_still_fatal() {
        // The tolerance is only for the *last* line. A malformed
        // line in the middle is still a corruption.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("s.jsonl");
        std::fs::write(&p, "not json\n{\"kind\":\"x\"}\n").unwrap();
        assert!(read_session(&p).is_err());
    }

    #[test]
    fn a_complete_file_round_trips_unchanged() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("s.jsonl");
        let rec = SessionRecorder::open(p.clone()).unwrap();
        rec.record(&SessionEntry::Approval {
            timestamp_ms: 1,
            holder: "h".into(),
            tool_name: "t".into(),
            decision: "approve".into(),
            edit: None,
        })
        .unwrap();
        rec.record(&SessionEntry::Approval {
            timestamp_ms: 2,
            holder: "h".into(),
            tool_name: "t".into(),
            decision: "deny".into(),
            edit: None,
        })
        .unwrap();
        assert_eq!(read_session(&p).unwrap().len(), 2);
    }
}

#[cfg(test)]
mod rehydrate_tests {
    use super::*;

    #[test]
    fn rehydrate_turns_empty_input_yields_no_messages() {
        let out = rehydrate_turns(&[], "session");
        assert!(out.is_empty());
    }

    #[test]
    fn rehydrate_turns_keeps_only_the_holder() {
        let entries = vec![
            SessionEntry::ToolCall {
                timestamp_ms: 1_000,
                holder: "session".to_string(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "a.rs"}),
                duration_ms: 5,
                result: serde_json::json!({"success": "contents of a.rs"}),
            },
            SessionEntry::ToolCall {
                timestamp_ms: 2_000,
                holder: "swarm:agent-3".to_string(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "b.rs"}),
                duration_ms: 5,
                result: serde_json::json!({"success": "contents of b.rs"}),
            },
        ];
        let out = rehydrate_turns(&entries, "session");
        assert_eq!(out.len(), 2, "one call => two messages (assistant + tool)");
        assert_eq!(out[0].tool_calls.len(), 1);
        assert_eq!(out[0].tool_calls[0].tool_name, "read_file");
        assert_eq!(out[1].role, kod_types::MessageRole::Tool);
        assert_eq!(out[1].content, "contents of a.rs");
    }

    #[test]
    fn rehydrate_turns_links_call_to_result_via_id() {
        let entries = vec![SessionEntry::ToolCall {
            timestamp_ms: 1_000,
            holder: "session".to_string(),
            tool_name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "fn main"}),
            duration_ms: 5,
            result: serde_json::json!({"success": "main.rs:1"}),
        }];
        let out = rehydrate_turns(&entries, "session");
        let call_id = out[0].tool_calls[0].id.clone();
        assert!(call_id.is_some(), "assistant call must carry an id");
        assert_eq!(
            out[1].tool_call_id, call_id,
            "tool result must reference the assistant's call id",
        );
    }

    #[test]
    fn rehydrate_turns_renders_error_results_with_a_prefix() {
        let entries = vec![SessionEntry::ToolCall {
            timestamp_ms: 1_000,
            holder: "session".to_string(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "missing.rs"}),
            duration_ms: 5,
            result: serde_json::json!({"error": "no such file"}),
        }];
        let out = rehydrate_turns(&entries, "session");
        assert_eq!(out[1].content, "Error: no such file");
    }

    #[test]
    fn rehydrate_turns_skips_non_toolcall_entries() {
        let entries = vec![
            SessionEntry::Cost {
                timestamp_ms: 1_000,
                holder: "session".to_string(),
                endpoint: "openai".to_string(),
                model: "gpt-4".to_string(),
                prompt_tokens: 100,
                completion_tokens: 20,
                cost_usd: 0.001,
            },
            SessionEntry::ToolCall {
                timestamp_ms: 2_000,
                holder: "session".to_string(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "a.rs"}),
                duration_ms: 5,
                result: serde_json::json!({"success": "x"}),
            },
        ];
        let out = rehydrate_turns(&entries, "session");
        assert_eq!(out.len(), 2, "cost entry contributes no messages");
    }

    #[test]
    fn rehydrate_turns_is_deterministic() {
        let entries = vec![SessionEntry::ToolCall {
            timestamp_ms: 1_000,
            holder: "session".to_string(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "a.rs"}),
            duration_ms: 5,
            result: serde_json::json!({"success": "contents"}),
        }];
        let a = rehydrate_turns(&entries, "session");
        let b = rehydrate_turns(&entries, "session");
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.role, y.role);
            assert_eq!(x.content, y.content);
            assert_eq!(x.tool_calls.len(), y.tool_calls.len());
        }
    }
}
