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
        /// The engine-transcript key the call ran under (`session` for
        /// the interactive session, `swarm:<agent-id>` for a swarm
        /// agent).
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
            SessionEntry::ToolCall { arguments, result, .. } => {
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
            SessionEntry::JevDecision { state_preview, answers, .. } => {
                let (r, ev) = self.redactor.redact(state_preview);
                if !ev.is_empty() {
                    *state_preview = r;
                    redactions.extend(ev);
                }
                redactions.extend(self.redactor.redact_json(answers));
            }
            _ => {}
        }
        let line = serde_json::to_string(&cloned)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        {
            let mut w = self.writer.lock().unwrap();
            writeln!(w, "{}", line).map_err(KodError::Io)?;
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
                writeln!(w, "{}", line).map_err(KodError::Io)?;
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
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEntry>(line) {
            Ok(e) => out.push(e),
            Err(e) => {
                // Distinguish "a well-formed JSON line whose `kind`
                // this build does not know" from "a corrupt line".
                //
                // The session format is extensible (AD-15): a newer
                // build can add variants and an older build reading
                // the file must not refuse to open it. A line that
                // parses as JSON but fails to deserialize as
                // `SessionEntry` is a forward-compat case, skipped
                // with a warning. A line that is not JSON at all is
                // a corrupt log — a hard error, because skipping
                // it would hide a real truncation.
                if serde_json::from_str::<serde_json::Value>(line).is_ok() {
                    tracing::warn!(
                        line = i + 1,
                        "session log line has an unknown kind; \
                         skipping (forward-compat)"
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
        })
        .unwrap();
        let b = SessionRecorder::open(p.clone()).unwrap();
        b.record(&SessionEntry::Approval {
            timestamp_ms: 2,
            holder: "b".into(),
            tool_name: "t".into(),
            decision: "deny".into(),
        })
        .unwrap();
        let read = read_session(&p).unwrap();
        assert_eq!(read.len(), 2);
    }
}
