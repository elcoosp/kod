//! Delta §14.1 coverage for the *secondary* provider call sites.
//!
//! The primary turn's prompt is built by `build_grounded_request`, which
//! reads the vault and obfuscates before the provider sees it. That path
//! has had end-to-end coverage since `secret_placeholder_integration`.
//!
//! Two *secondary* prompt paths bypass `build_grounded_request`:
//!
//! - `maybe_create_plan` (plans.rs) sends the raw user input to the plan
//!   provider when the task classifies as Complex/MultiStep and the
//!   input is at least 30 chars. A vault-registered secret pasted into
//!   the prompt reached the plan provider as plaintext.
//!
//! - The summary branch in `process_for` (process.rs) sends raw tool
//!   results to the summary provider when the model made tool calls and
//!   never wrote back.
//!
//! 110bbc84 threaded the vault through both. This file pins the fix so a
//! refactor cannot reintroduce the leak.
//!
//! **Reverse test:** revert `110bbc84^:crates/kod-core/src/engine/plans.rs`,
//! rebuild, run this test — it must fail with the raw secret visible in
//! the assertion message. If it passes with the fix reverted, the test
//! does not exercise the plan path and is vacuous.

use async_trait::async_trait;
use futures::Stream;
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::testkit::{MockProvider, Script};
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities,
    ProviderRegistry, StreamChunk,
};
use kod_types::secret_placeholder::SecretVault;
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// Captures every prompt the engine sends to the provider — both the
/// raw-string `generate` calls (secondary paths: plan, review, summary)
/// and the structured `complete` calls (the primary turn).
struct CapturingProvider {
    generate_prompts: Arc<Mutex<Vec<String>>>,
    complete_requests: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl CapturingProvider {
    fn new() -> (
        Self,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<Vec<CompletionRequest>>>,
    ) {
        let gen_p = Arc::new(Mutex::new(Vec::new()));
        let cmp_r = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                generate_prompts: Arc::clone(&gen_p),
                complete_requests: Arc::clone(&cmp_r),
            },
            gen_p,
            cmp_r,
        )
    }
}

#[async_trait]
impl LlmProvider for CapturingProvider {
    fn name(&self) -> &str {
        "capturing"
    }

    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }

    async fn generate(&self, p: &str, _o: &GenerationOptions) -> Result<String> {
        self.generate_prompts.lock().unwrap().push(p.to_string());
        // `"[]"` parses as an empty JSON array for `parse_plan_steps`
        // (the plan is not set, but the generate call is already
        // captured). Every other secondary caller (summary, review)
        // also accepts any text.
        Ok("[]".into())
    }

    async fn generate_with_tools(
        &self,
        p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        self.generate_prompts.lock().unwrap().push(p.to_string());
        Ok(GenerationResponse::Text {
            content: "[]".into(),
            usage: None,
        })
    }

    fn stream(
        &self,
        p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        self.generate_prompts.lock().unwrap().push(p.to_string());
        Box::pin(futures::stream::empty())
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        self.complete_requests.lock().unwrap().push(req.clone());
        Ok(GenerationResponse::Text {
            content: "(done)".into(),
            usage: None,
        })
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

async fn engine_with(provider: Arc<CapturingProvider>) -> (Arc<KodEngine>, TempDir) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_provider_does_not_see_raw_secrets() {
    let (provider, gen_prompts, complete_reqs) = CapturingProvider::new();
    let provider: Arc<CapturingProvider> = Arc::new(provider);
    let (engine, _tmp) = engine_with(Arc::clone(&provider)).await;

    // Install a vault with one known secret.
    let vault = Arc::new(SecretVault::with_random_key());
    let raw_secret = "AKIAIOSFODNN7EXAMPLE";
    let placeholder = vault.register(raw_secret);
    assert_ne!(
        placeholder, raw_secret,
        "sanity: register must return a placeholder, not the raw value",
    );
    engine.set_secret_vault(Arc::clone(&vault)).await;

    // A Complex-classified input (contains "design") long enough to pass
    // `maybe_create_plan`'s 30-char gate, containing the raw secret.
    let input = format!(
        "design a deployment pipeline for the service that uses the credential {raw_secret} for authentication"
    );
    assert!(input.len() >= 30, "input must pass the length gate");
    assert!(input.contains(raw_secret), "input must contain the secret");

    engine
        .process_for("session-1", &input)
        .await
        .expect("process_for");

    // -------- captured evidence --------
    let generates = gen_prompts.lock().unwrap().clone();
    let cmp = complete_reqs.lock().unwrap().clone();
    println!("captured generate prompts: {}", generates.len());
    println!("captured complete requests: {}", cmp.len());
    for (i, p) in generates.iter().enumerate() {
        println!("--- generate prompt #{i} ({}) ---", p.len());
        println!("{p}");
    }

    // -------- defensive: did the plan path actually fire? --------
    let plan_prompt = generates
        .iter()
        .find(|p| p.contains("Produce a concise, ordered plan"))
        .unwrap_or_else(|| {
            panic!(
                "plan path was NOT triggered — {} generate() calls captured, none of them \
                 a plan prompt. Either the classifier did not route to Complex, or \
                 `create_plan` was false in prepare.rs. Check the gating before trusting \
                 this test. Input was: {input}",
                generates.len(),
            );
        });

    // -------- assertions --------
    // 1. No generate prompt leaks the raw secret.
    for (i, p) in generates.iter().enumerate() {
        assert!(
            !p.contains(raw_secret),
            "generate prompt #{i} leaked the raw secret:\n{p}",
        );
    }

    // 2. The plan prompt specifically contains the placeholder, proving
    //    the obfuscation actually ran on that path (not just that the
    //    secret was absent because the prompt never got built).
    assert!(
        plan_prompt.contains(&placeholder),
        "plan prompt must contain the placeholder form, got:\n{plan_prompt}",
    );

    // 3. No primary `complete` request leaks the raw secret. That was
    //    already true before 110bbc84; pinning it here means a future
    //    refactor of the plan path cannot regress the primary one.
    for (i, req) in cmp.iter().enumerate() {
        for seg in &req.system.segments {
            assert!(
                !seg.text.contains(raw_secret),
                "complete request #{i} system segment leaked raw secret:\n{}",
                seg.text,
            );
        }
        for m in &req.messages {
            assert!(
                !m.content.contains(raw_secret),
                "complete request #{i} message leaked raw secret:\n{}",
                m.content,
            );
        }
    }
}

/// The summary branch (`process.rs:657`) fires when the model made tool
/// calls and then produced empty text. The summary prompt is built from
/// `convo` (the pre-loop prompt, which contains the raw user input) plus
/// serialized tool results. `110bbc84` obfuscates the prompt before the
/// summary provider sees it; this test pins that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_provider_does_not_see_raw_secrets() {
    let tmp = TempDir::new().unwrap();

    // The secret is in the FILE CONTENT, not in the user input. `read_file`
    // returns it as tool output; that output is what the summary branch
    // serializes into the prompt. If the summary obfuscation were absent,
    // the raw secret would flow from the file through the tool result to
    // the summary provider.
    let secret_for_file = "AKIAIOSFODNN7EXAMPLE";
    std::fs::write(tmp.path().join("creds.txt"), secret_for_file).unwrap();

    // Round 1: read_file. Round 2: empty text -> triggers the summary
    // branch (final_text.trim().is_empty() && !tool_calls.is_empty()).
    let tool_call = ToolCall {
        id: Some("call-summary".into()),
        tool_name: "read_file".into(),
        arguments: serde_json::json!({ "path": "creds.txt" }),
    };
    let provider_concrete: Arc<MockProvider> = Arc::new(MockProvider::scripted(
        "mock",
        vec![Script::Calls(vec![tool_call]), Script::Text(String::new())],
    ));
    let provider_dyn: Arc<dyn LlmProvider> = provider_concrete.clone();

    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("s2.kod")).unwrap());
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "mock",
        provider_dyn,
        ProviderCapabilities::conservative(),
        "mock-model",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("mock", "mock-model"), None)
        .await;
    engine.start().await.unwrap();

    let vault = Arc::new(SecretVault::with_random_key());
    let placeholder = vault.register(secret_for_file);
    assert_ne!(
        placeholder, secret_for_file,
        "sanity: register returns placeholder"
    );
    engine.set_secret_vault(Arc::clone(&vault)).await;

    let input = "read the file creds.txt and tell me what it says";
    engine
        .process_for("session", input)
        .await
        .expect("process_for");

    let captured = provider_concrete.captured_prompts.lock().unwrap().clone();
    println!("captured generate prompts: {}", captured.len());
    for (i, p) in captured.iter().enumerate() {
        println!("--- prompt {i} ({}) ---", p.len());
        println!("{p}");
    }

    let summary_prompt = captured
        .iter()
        .find(|p| p.contains("Summarize what you did"))
        .unwrap_or_else(|| {
            panic!(
                "summary path was NOT triggered — {} generate() prompts captured, \
                 none contained 'Summarize what you did'. Possible causes: \
                 (1) read_file denied by default ToolPermissions; \
                 (2) Script::Calls did not produce a tool call; \
                 (3) round-2 empty text became non-empty. Captured prompts:\n{:#?}",
                captured.len(),
                captured
            );
        });

    for (i, p) in captured.iter().enumerate() {
        assert!(
            !p.contains(secret_for_file),
            "prompt {i} leaked the raw secret from the tool result:\n{p}"
        );
    }

    assert!(
        summary_prompt.contains(&placeholder),
        "summary prompt must contain the placeholder for the tool-result secret, got:\n{summary_prompt}",
    );
}
