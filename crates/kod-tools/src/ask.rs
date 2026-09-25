//! `ask_user`: pause the agentic loop and get a plain-text answer from
//! the user.
//!
//! The engine's streaming loop already knows how to pause on a oneshot
//! (the approval flow). `ask_user` reuses the same mechanism with a
//! different marker: the tool emits `\0kod-question:<id>:<json>\0` on
//! the chunk channel, registers a oneshot in the engine's pending map,
//! and awaits the answer. The consumer (TUI or CLI) renders the
//! question and calls `respond_to_question(id, answer)`.
//!
//! The tool itself is thin: it holds an `Arc<RwLock<HashMap<u64,
//! oneshot::Sender<String>>>>` and a counter, plus a reference to the
//! `chunk_tx` the engine was called with. But `Tool::execute` does not
//! receive the chunk_tx — the tool would have to reach into the engine,
//! which is the wrong direction. Instead, the engine intercepts calls
//! to `ask_user` *before* dispatching to the tool: it does the marker
//! emission and the await itself, then hands the answer back as the
//! tool's result. `AskUserTool::execute` is therefore a fallback that
//! returns an error if it is ever called directly (in a test, from a
//! non-streaming context) — a loud failure beats a silent hang.

use crate::{Tool, ToolContext};
use kod_error::Result;
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// Marker prefix for a question on the streaming chunk channel:
/// `\0kod-question:<id>:<json>\0`. The JSON shape is
/// `{"question": "...", "placeholder": "..."}`.
pub const QUESTION_MARKER: &str = "\0kod-question:";

pub fn question_marker(id: u64, question_json: &str) -> String {
    let clean = question_json.replace('\0', " ");
    format!("{QUESTION_MARKER}{id}:{clean}\0")
}

pub fn parse_question(chunk: &str) -> Option<(u64, &str)> {
    let rest = chunk.strip_prefix(QUESTION_MARKER)?;
    let body = rest.strip_suffix('\0')?;
    let (id_str, json) = body.split_once(':')?;
    Some((id_str.parse().ok()?, json))
}

/// The serialized question. Mirrors the approval request shape.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QuestionRequest {
    pub question: String,
    #[serde(default)]
    pub placeholder: Option<String>,
}

pub struct AskUserTool {
    pub definition: ToolDefinition,
}

impl AskUserTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "ask_user".to_string(),
                description: "Ask the user a question and wait for the answer. Use this \
                    when the task genuinely requires a decision only the user can make \
                    (a preference, an unknown fact, a fork in the plan). Do NOT use it \
                    to ask permission — the engine's approval flow covers writes."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask, one sentence."
                        },
                        "placeholder": {
                            "type": "string",
                            "description": "Optional hint about the expected answer format."
                        }
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
                load_mode: Default::default(),
            },
        }
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, _params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        // The engine intercepts calls to `ask_user` before dispatching
        // to this tool (see `KodEngine::run_tool_calls`). If we get
        // here, the call came from a context without a chunk_tx — a
        // non-streaming `process` call, a direct test.
        Ok(ToolResult::Error(
            "ask_user requires an interactive consumer and cannot be called from a \
             non-streaming context. Use `kod tui` or `kod chat`."
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_marker_roundtrips() {
        let req = QuestionRequest {
            question: "Which database?".to_string(),
            placeholder: Some("postgres / sqlite".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let chunk = question_marker(42, &json);
        let (id, parsed) = parse_question(&chunk).unwrap();
        assert_eq!(id, 42);
        let decoded: QuestionRequest = serde_json::from_str(parsed).unwrap();
        assert_eq!(decoded.question, "Which database?");
        assert_eq!(decoded.placeholder.as_deref(), Some("postgres / sqlite"));
    }

    #[test]
    fn question_marker_rejects_other_chunks() {
        assert!(parse_question("plain").is_none());
        assert!(parse_question("\0kod-approval:1:{}").is_none());
    }

    #[test]
    fn question_marker_sanitizes_nul() {
        let chunk = question_marker(1, "{\"question\":\"a\0b\"}");
        let (_id, body) = parse_question(&chunk).unwrap();
        assert!(!body.contains('\0'));
    }
}

#[cfg(test)]
mod coverage_question_request {
    //! `QuestionRequest` is the payload of a question marker. A
    //! regression that changed the field names would break the
    //! marker round-trip between the engine and the TUI without
    //! any error pointing at the cause.
    use super::*;

    #[test]
    fn question_request_round_trips_with_placeholder() {
        let req = QuestionRequest {
            question: "Which branch?".into(),
            placeholder: Some("main / develop".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: QuestionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.question, req.question);
        assert_eq!(parsed.placeholder, req.placeholder);
    }

    #[test]
    fn question_request_parses_without_placeholder() {
        // A model that omits `placeholder` must not fail the parse;
        // the field is `#[serde(default)]` for exactly this case.
        let json = r#"{"question":"Which branch?"}"#;
        let req: QuestionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.question, "Which branch?");
        assert!(req.placeholder.is_none());
    }

    #[test]
    fn a_question_marker_with_unicode_round_trips() {
        // Question text may contain any UTF-8 a user typed. The
        // marker's NUL-sanitization must not corrupt the bytes.
        let req = QuestionRequest {
            question: "¿Cuál es la rama — main o dev? 🚀".into(),
            placeholder: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let chunk = question_marker(7, &json);
        let (id, body) = parse_question(&chunk).unwrap();
        assert_eq!(id, 7);
        let parsed: QuestionRequest = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.question, req.question);
    }

    #[test]
    fn a_question_with_a_placeholder_can_be_empty_string() {
        // The schema says `placeholder: Option<String>`; an empty
        // string is distinct from `None` and must be preserved.
        let req = QuestionRequest {
            question: "q".into(),
            placeholder: Some(String::new()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: QuestionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.placeholder.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn ask_user_tool_reports_a_clean_error_outside_the_streaming_path() {
        // The tool's execute is a fallback for the non-streaming
        // case. It must return a `ToolResult::Error` with an
        // actionable message — not panic, not an `Err(KodError)`,
        // because the engine turns `Err` into a hard failure while
        // `Error` reaches the model as a tool-level refusal.
        use crate::{Tool, ToolContext};
        use kod_types::ToolPermissions;
        let tool = AskUserTool::new();
        let ctx = ToolContext::new("/tmp").with_permissions(ToolPermissions::default());
        let r = tool
            .execute(&serde_json::json!({"question": "hi"}), &ctx)
            .await
            .unwrap();
        match r {
            kod_types::ToolResult::Error(msg) => {
                assert!(msg.contains("interactive"), "got: {msg}");
                assert!(
                    msg.contains("kod tui") || msg.contains("kod chat"),
                    "got: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }
}
