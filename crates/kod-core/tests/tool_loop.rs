//! Engine tool-calling loop: a scripted provider requests `list_files`,
//! the engine executes it against the working dir and feeds the result back.

use async_trait::async_trait;
use kod_core::engine::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolCall;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

mod common;

/// Scripted provider: first round requests a real `list_files` call,
/// second round answers in text.
struct ScriptedProvider {
    rounds: Mutex<usize>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn list_models(&self) -> kod_error::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn generate(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> kod_error::Result<String> {
        Ok("final summary".to_string())
    }

    async fn generate_with_tools(
        &self,
        _prompt: &str,
        _tools: &[kod_types::ToolDefinition],
        _options: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        let mut rounds = self.rounds.lock().unwrap();
        *rounds += 1;
        if *rounds == 1 {
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    tool_name: "list_files".to_string(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                usage: None,
            })
        } else {
            Ok(GenerationResponse::Text {
                content: "saw the files".to_string(),
                usage: None,
            })
        }
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn test_engine_tool_loop_lists_working_dir() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(temp_dir.path().join("marker.txt"), "x").unwrap();

    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();
    common::install_test_provider(&engine, Arc::new(ScriptedProvider {
            rounds: Mutex::new(0),
        })).await;

    let response = engine.process("what is here?").await.unwrap();

    // One tool call executed, one result recorded, text answer produced.
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].tool_name, "list_files");
    assert_eq!(response.tool_results.len(), 1);
    let rendered = format!("{:?}", response.tool_results[0]);
    assert!(
        rendered.contains("marker.txt"),
        "tool result should list the temp dir file, got: {rendered}"
    );
    assert_eq!(response.text.as_deref(), Some("saw the files"));
}

#[tokio::test]
async fn test_process_streaming_delivers_chunks_and_tool_marker() {
    use kod_core::engine::parse_tool_start;

    let temp_dir = TempDir::new().unwrap();
    std::fs::write(temp_dir.path().join("marker.txt"), "x").unwrap();

    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("stream.redb")).unwrap();
    engine.start().await.unwrap();
    common::install_test_provider(&engine, Arc::new(ScriptedProvider {
            rounds: Mutex::new(0),
        })).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let response = engine
        .process_streaming("what is here?", &tx)
        .await
        .unwrap();
    drop(tx);

    let mut texts = Vec::new();
    let mut tools = Vec::new();
    while let Some(chunk) = rx.recv().await {
        if let Some(name) = parse_tool_start(&chunk) {
            tools.push(name.to_string());
        } else {
            texts.push(chunk);
        }
    }

    assert_eq!(tools, vec!["list_files".to_string()]);
    assert!(
        texts.concat().contains("saw the files"),
        "streamed text should carry the final answer"
    );
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.text.as_deref(), Some("saw the files"));
}

#[tokio::test]
async fn test_tool_markers_roundtrip_and_brief() {
    use kod_core::engine::{
        format_call_brief, parse_tool_args, parse_tool_start, tool_args_marker, tool_start_marker,
    };

    let m = tool_start_marker("execute_command");
    assert_eq!(parse_tool_start(&m), Some("execute_command"));
    assert_eq!(parse_tool_start("plain text"), None);

    let a = tool_args_marker("execute_command cargo test -p kod-tui");
    assert_eq!(
        parse_tool_args(&a),
        Some("execute_command cargo test -p kod-tui")
    );
    assert_eq!(parse_tool_args("plain text"), None);

    // The running line shows the command itself, first line only.
    let brief = format_call_brief(
        "execute_command",
        &serde_json::json!({"command": "cargo test -p kod-tui\ncargo clippy"}),
    );
    assert_eq!(brief, "execute_command cargo test -p kod-tui");
    let brief = format_call_brief(
        "read_file",
        &serde_json::json!({"path": "crates/kod-tui/src/app.rs"}),
    );
    assert!(
        brief.contains("app.rs"),
        "file excerpt should name the file, got: {brief}"
    );
    let brief = format_call_brief("list_files", &serde_json::json!({}));
    assert_eq!(brief, "list_files");
}

/// Provider that records every prompt: round 1 requests a tool, later
/// rounds answer in text.
struct CapturingProvider {
    rounds: Mutex<usize>,
    prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl LlmProvider for CapturingProvider {
    fn name(&self) -> &str {
        "capturing"
    }

    async fn list_models(&self) -> kod_error::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn generate(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> kod_error::Result<String> {
        Ok("summary".to_string())
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        _tools: &[kod_types::ToolDefinition],
        _options: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        let mut rounds = self.rounds.lock().unwrap();
        *rounds += 1;
        if *rounds == 1 {
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    tool_name: "list_files".to_string(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                usage: None,
            })
        } else {
            Ok(GenerationResponse::Text {
                content: "done".to_string(),
                usage: None,
            })
        }
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn test_steer_note_reaches_next_round() {
    let temp_dir = TempDir::new().unwrap();
    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("steer.redb")).unwrap();
    engine.start().await.unwrap();
    let provider = Arc::new(CapturingProvider {
        rounds: Mutex::new(0),
        prompts: Mutex::new(Vec::new()),
    });
    {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            provider.clone(),
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        engine
            .set_registry(
                std::sync::Arc::new(reg),
                kod_provider::ModelRef::new("default", ""),
                None,
            )
            .await;
    }

    engine.steer("focus on src only").await;
    engine.process("list things").await.unwrap();

    let prompts = provider.prompts.lock().unwrap();
    assert!(
        prompts.len() >= 2,
        "tool round should feed back for a second pass"
    );
    assert!(
        prompts[1].contains("focus on src only") && prompts[1].contains("User steer"),
        "steer note must be injected after the tool round, got: {}",
        prompts[1]
    );
}

#[tokio::test]
async fn test_cancel_stops_process() {
    let temp_dir = TempDir::new().unwrap();
    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("cancel.redb")).unwrap();
    engine.start().await.unwrap();
    common::install_test_provider(&engine, Arc::new(ScriptedProvider {
            rounds: Mutex::new(0),
        })).await;

    assert!(!engine.is_cancelled());
    engine.request_cancel();
    assert!(engine.is_cancelled());
    let err = engine.process("anything").await.unwrap_err();
    assert!(
        err.to_string().contains("cancelled by user"),
        "unexpected error: {err}"
    );
    engine.clear_cancel();
    assert!(!engine.is_cancelled());
}

/// Provider for the goal loop: one tool round, then GOAL MET.
struct GoalProvider {
    rounds: Mutex<usize>,
}

#[async_trait]
impl LlmProvider for GoalProvider {
    fn name(&self) -> &str {
        "goal"
    }

    async fn list_models(&self) -> kod_error::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn generate(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> kod_error::Result<String> {
        Ok("summary".to_string())
    }

    async fn generate_with_tools(
        &self,
        _prompt: &str,
        _tools: &[kod_types::ToolDefinition],
        _options: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        let mut rounds = self.rounds.lock().unwrap();
        *rounds += 1;
        if *rounds == 1 {
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    tool_name: "list_files".to_string(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                usage: None,
            })
        } else {
            Ok(GenerationResponse::Text {
                content: "everything is done\nGOAL MET".to_string(),
                usage: None,
            })
        }
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn test_goal_loop_stops_at_goal_met() {
    use kod_core::engine::parse_tool_start;

    let temp_dir = TempDir::new().unwrap();
    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("goal.redb")).unwrap();
    engine.start().await.unwrap();
    common::install_test_provider(&engine, Arc::new(GoalProvider {
            rounds: Mutex::new(0),
        })).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let response = engine
        .process_goal_streaming("tidy up", "leave no stray files", &tx)
        .await
        .unwrap();
    drop(tx);

    let mut chunks = Vec::new();
    while let Some(chunk) = rx.recv().await {
        if parse_tool_start(&chunk).is_none() && kod_core::engine::parse_tool_args(&chunk).is_none()
        {
            chunks.push(chunk);
        }
    }
    let stream_text = chunks.concat();
    assert!(
        !stream_text.contains("turn 2"),
        "loop should stop after GOAL MET, streamed: {stream_text}"
    );
    let text = response.text.unwrap_or_default();
    assert!(
        text.contains("GOAL MET"),
        "goal text should survive, got: {text}"
    );
    assert_eq!(response.tool_calls.len(), 1);
}

#[tokio::test]
async fn test_skill_details_lists_descriptions() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("alpha.md"),
        "---\nname: alpha\ndescription: Alpha does things\nversion: 1.0.0\ncategory: test\n---\n\n## Instructions\n\nDo alpha.\n",
    )
    .unwrap();

    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("skills.redb")).unwrap();
    engine.start().await.unwrap();
    let loaded = engine.load_skills(&skills_dir).await.unwrap();
    assert_eq!(loaded, 1);

    let details = engine.loaded_skill_details().await;
    assert_eq!(
        details,
        vec![("alpha".to_string(), "Alpha does things".to_string())]
    );
}

#[tokio::test]
async fn test_second_turn_sees_first_turn_history() {
    // Regression: every prompt used to carry only the current input, so the
    // model opened with "this is a fresh conversation" mid-session (or right
    // after a compact). The second prompt must contain turn one's text.
    let temp_dir = TempDir::new().unwrap();
    let config = RouterConfig {
        context_window: 8192,
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };
    let engine = KodEngine::new(config, temp_dir.path().join("hist.redb")).unwrap();
    engine.start().await.unwrap();
    let provider = Arc::new(CapturingProvider {
        rounds: Mutex::new(1),
        prompts: Mutex::new(Vec::new()),
    });
    {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            provider.clone(),
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        engine
            .set_registry(
                std::sync::Arc::new(reg),
                kod_provider::ModelRef::new("default", ""),
                None,
            )
            .await;
    }

    engine.process("first question about apples").await.unwrap();
    engine.process("and what about pears?").await.unwrap();

    // Clone the captured prompts out — holding the provider's std Mutex
    // across a later `process().await` would self-deadlock (the provider
    // locks the same mutex on every call).
    let prompts: Vec<String> = provider.prompts.lock().unwrap().clone();
    assert!(
        prompts.len() >= 2,
        "expected two prompts, got {}",
        prompts.len()
    );
    let second = prompts.last().unwrap();
    assert!(
        second.contains("first question about apples"),
        "second prompt lost the first user turn: {second}"
    );
    assert!(
        second.contains("## Conversation so far"),
        "history section missing from second prompt: {second}"
    );

    // `/clear` forgets the transcript: the next prompt starts over.
    engine.clear_history().await;
    engine.process("something entirely new").await.unwrap();
    let prompts: Vec<String> = provider.prompts.lock().unwrap().clone();
    let third = prompts.last().unwrap();
    assert!(
        !third.contains("first question about apples"),
        "cleared history leaked into next prompt: {third}"
    );
}
