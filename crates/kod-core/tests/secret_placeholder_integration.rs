//! Delta §14.1 end-to-end: a secret in the transcript is replaced
//! with a placeholder before reaching the provider, and a
//! placeholder in a tool call's arguments is replaced with the raw
//! secret before the tool runs.
//!
//! The two directions are the whole mechanism, and neither is
//! provable by unit test — the unit tests exercise the vault in
//! isolation, but the vault has no effect unless the engine calls
//! it at the right two sites. These tests exercise those sites.

use async_trait::async_trait;
use futures::Stream;
use kod_core::router::RouterConfig;
use kod_core::KodEngine;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef,
    ProviderCapabilities, ProviderRegistry, StreamChunk,
};
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A provider that records every request it sees and replays a
/// scripted sequence of responses.
struct RecordingProvider {
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
    scripted: Mutex<Vec<GenerationResponse>>,
}

impl RecordingProvider {
    fn new() -> (Self, Arc<Mutex<Vec<CompletionRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                seen: Arc::clone(&seen),
                scripted: Mutex::new(Vec::new()),
            },
            seen,
        )
    }

    fn script(&self, response: GenerationResponse) {
        self.scripted.lock().unwrap().push(response);
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok("(recorded)".into())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: "(recorded)".into(),
            usage: None,
        })
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        self.seen.lock().unwrap().push(req.clone());
        let mut s = self.scripted.lock().unwrap();
        if s.is_empty() {
            Ok(GenerationResponse::Text {
                content: "(done)".into(),
                usage: None,
            })
        } else {
            Ok(s.remove(0))
        }
    }
}

fn fixture_config(dir: &std::path::Path) -> RouterConfig {
    RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    }
}

async fn engine_with(
    provider: Arc<RecordingProvider>,
) -> (Arc<KodEngine>, TempDir) {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("s.kod")).unwrap());
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider as Arc<dyn LlmProvider>,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();
    (engine, tmp)
}

// ---------------------------------------------------------------------------
// Outbound: a secret in the transcript is obfuscated before the provider
// sees it.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_in_the_user_message_is_obfuscated_before_the_provider_sees_it() {
    let (provider, seen) = RecordingProvider::new();
    let (engine, _tmp) = engine_with(Arc::new(provider)).await;

    let vault = Arc::new(kod_types::secret_placeholder::SecretVault::with_key(
        [1u8; 32],
    ));
    let placeholder = vault.register("super-secret-value-1234");
    engine.set_secret_vault(vault).await;

    // Drive one turn whose user message contains the raw secret.
    let _ = engine
        .process("please remember the key super-secret-value-1234 for later")
        .await;

    let requests = seen.lock().unwrap();
    assert!(!requests.is_empty(), "the provider must have been called");
    // Every message in every request must be placeholder-only — the
    // raw string must not appear anywhere the provider could see it.
    for req in requests.iter() {
        for m in &req.messages {
            assert!(
                !m.content.contains("super-secret-value-1234"),
                "raw secret leaked into a message: {:?}",
                m.content,
            );
        }
        for seg in &req.system.segments {
            assert!(
                !seg.text.contains("super-secret-value-1234"),
                "raw secret leaked into a system segment",
            );
        }
    }
    // And at least one message must carry the placeholder, proving
    // the substitution actually ran (as opposed to the secret having
    // been dropped).
    let saw_placeholder = requests.iter().any(|r| {
        r.messages.iter().any(|m| m.content.contains(&placeholder))
    });
    assert!(
        saw_placeholder,
        "no message carried the placeholder {placeholder:?}; \
         the obfuscation did not run",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_vault_the_raw_secret_reaches_the_provider() {
    // Control: the pre-§14.1 behaviour must be exactly what a
    // default engine does. A no-vault engine sends the raw bytes.
    let (provider, seen) = RecordingProvider::new();
    let (engine, _tmp) = engine_with(Arc::new(provider)).await;
    // No `set_secret_vault` call.
    let _ = engine
        .process("the key is super-secret-value-1234")
        .await;
    let requests = seen.lock().unwrap();
    let saw_raw = requests
        .iter()
        .any(|r| r.messages.iter().any(|m| m.content.contains("super-secret-value-1234")));
    assert!(
        saw_raw,
        "a vault-less engine must send raw bytes (the pre-§14.1 shape)",
    );
}

// ---------------------------------------------------------------------------
// Inbound: a placeholder in a tool call's arguments is deobfuscated
// before the tool runs.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_placeholder_in_a_write_file_call_writes_the_raw_secret() {
    let (provider, _seen) = RecordingProvider::new();
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("s.kod")).unwrap());

    // Build a vault with one known secret, remember its placeholder.
    let vault = Arc::new(kod_types::secret_placeholder::SecretVault::with_key(
        [1u8; 32],
    ));
    let placeholder = vault.register("super-secret-value-1234");
    engine.set_secret_vault(vault).await;

    // Script the provider: first call returns a write_file tool call
    // whose arguments contain the *placeholder* (as a real model
    // would, since that is the only form it ever saw); the second
    // call returns a plain text reply so the loop terminates.
    provider.script(GenerationResponse::ToolCalls {
        calls: vec![ToolCall {
            id: Some("call-1".into()),
            tool_name: "write_file".into(),
            arguments: serde_json::json!({
                "path": "note.txt",
                "content": format!("API_KEY={placeholder}"),
            }),
        }],
        usage: None,
    });
    provider.script(GenerationResponse::Text {
        content: "done".into(),
        usage: None,
    });

    // Register the provider and drive the turn.
    let provider = Arc::new(provider);
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider as Arc<dyn LlmProvider>,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    let _ = engine.process("write the api key to note.txt").await;

    // The tool wrote the file. Assert the file contains the RAW
    // secret (deobfuscation ran) and not the placeholder.
    let written = std::fs::read_to_string(tmp.path().join("note.txt"))
        .expect("write_file must have created note.txt");
    assert!(
        written.contains("super-secret-value-1234"),
        "the tool must have written the raw secret; got {written:?}",
    );
    assert!(
        !written.contains(&placeholder),
        "the placeholder must have been substituted; got {written:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_vault_a_placeholder_passes_through_unchanged() {
    // Control: no vault means no deobfuscation. A file written with
    // a placeholder-shaped string is written literally.
    let (provider, _seen) = RecordingProvider::new();
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("s.kod")).unwrap());
    // No `set_secret_vault` call.

    provider.script(GenerationResponse::ToolCalls {
        calls: vec![ToolCall {
            id: Some("call-1".into()),
            tool_name: "write_file".into(),
            arguments: serde_json::json!({
                "path": "note.txt",
                "content": "API_KEY=«Credential-deadbeefdeadbeef»",
            }),
        }],
        usage: None,
    });
    provider.script(GenerationResponse::Text {
        content: "done".into(),
        usage: None,
    });

    let provider = Arc::new(provider);
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider as Arc<dyn LlmProvider>,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    let _ = engine.process("write something").await;
    let written = std::fs::read_to_string(tmp.path().join("note.txt"))
        .expect("write_file must have created note.txt");
    // The unknown placeholder passes through unchanged: the model
    // invented a placeholder shape and the vault correctly declines
    // to give it a secret it did not have.
    assert!(
        written.contains("«Credential-deadbeefdeadbeef»"),
        "an unknown placeholder must pass through; got {written:?}",
    );
}
