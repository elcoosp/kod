//! Per-transcript working directory override (D4-D1).
//!
//! The engine exposes a setter that a swarm runner uses to point an
//! agent's transcript at its own worktree. Tools called on that
//! transcript must resolve paths against the override, not the
//! engine-wide root.

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[path = "common/install_test_provider.rs"]
mod install_test_provider_mod;
use install_test_provider_mod::install_test_provider;

struct ScriptedProvider {
    calls: Mutex<Vec<Vec<ToolCall>>>,
}

impl ScriptedProvider {
    fn new(calls: Vec<Vec<ToolCall>>) -> Self {
        Self {
            calls: Mutex::new(calls),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok("done".into())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let mut g = self.calls.lock().unwrap();
        let calls = if g.is_empty() {
            Vec::new()
        } else {
            g.remove(0)
        };
        if calls.is_empty() {
            Ok(GenerationResponse::Text {
                content: "done".into(),
                usage: None,
            })
        } else {
            Ok(GenerationResponse::ToolCalls { calls, usage: None })
        }
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

fn engine_in(dir: &std::path::Path) -> KodEngine {
    std::fs::write(dir.join("main.rs"), "pub fn main() {}\n").unwrap();
    let db_path = dir.join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, db_path).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_with_override_writes_under_the_override() {
    let tmp = TempDir::new().unwrap();
    let engine_root = tmp.path().join("engine_root");
    let worktree = tmp.path().join("agent_wt");
    std::fs::create_dir_all(&engine_root).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();

    let engine = engine_in(&engine_root);
    let provider = Arc::new(ScriptedProvider::new(vec![vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "out.txt",
            "content": "worktree content\n",
        }),
    }]]));
    install_test_provider(&engine, provider).await;
    engine.start().await.unwrap();

    let key = "swarm:agent-1";
    engine
        .set_transcript_working_dir(key, Some(worktree.clone()))
        .await;

    // Run a prompt on that transcript.
    let _ = engine.process_for(key, "write out.txt").await.unwrap();

    // The file should land under the worktree, not the engine root.
    assert!(
        worktree.join("out.txt").is_file(),
        "write should land under the override"
    );
    assert!(
        !engine_root.join("out.txt").exists(),
        "engine root should not have received the write"
    );

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_transcript_still_uses_engine_root() {
    let tmp = TempDir::new().unwrap();
    let engine = engine_in(tmp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "out.txt",
            "content": "default content\n",
        }),
    }]]));
    install_test_provider(&engine, provider).await;
    engine.start().await.unwrap();

    let _ = engine.process("write out.txt").await.unwrap();
    assert!(tmp.path().join("out.txt").is_file());
    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_override_reverts_to_engine_root() {
    let tmp = TempDir::new().unwrap();
    let engine_root = tmp.path().join("engine_root");
    let worktree = tmp.path().join("wt");
    std::fs::create_dir_all(&engine_root).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();

    let engine = engine_in(&engine_root);
    let provider1 = Arc::new(ScriptedProvider::new(vec![vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "first.txt",
            "content": "in worktree\n",
        }),
    }]]));
    install_test_provider(&engine, provider1).await;
    engine.start().await.unwrap();

    let key = "swarm:agent-1";
    engine
        .set_transcript_working_dir(key, Some(worktree.clone()))
        .await;
    let _ = engine.process_for(key, "write first.txt").await.unwrap();
    assert!(worktree.join("first.txt").is_file());

    engine.clear_transcript_working_dir(key).await;
    let provider2 = Arc::new(ScriptedProvider::new(vec![vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "second.txt",
            "content": "in root\n",
        }),
    }]]));
    install_test_provider(&engine, provider2).await;
    let _ = engine.process_for(key, "write second.txt").await.unwrap();
    assert!(
        engine_root.join("second.txt").is_file(),
        "after clearing, writes should land in engine root"
    );

    engine.shutdown().await.unwrap();
}
