    //! P5.6 — the mid-stream switch mechanism.
    //!
    //! `fallback_stream_for_off_track` opens a stream against a
    //! fallback endpoint and hands back the provider, its
    //! `ModelRef`, and a `'static` boxed stream. This pins:
    //!
    //! * a resolvable fallback produces a stream that yields the
    //!   fallback provider's chunks;
    //! * an unresolvable `ModelRef` returns `None` (no panic, no
    //!   stream).
    //!
    //! The full path from an `OffTrack` verdict to the swap lives
    //! inside `stream_round` and needs a scripted Jev response to
    //! exercise. That test waits on a Jev test abstraction; the
    //! mechanism test below covers the load-bearing half.
    use super::*;
    use futures::StreamExt;
    use kod_provider::{
        CompletionRequest, GenerationOptions, GenerationResponse, LlmProvider,
        ProviderCapabilities, ProviderRegistry, StreamChunk,
    };
    use std::sync::Arc;

    /// A provider that answers every streaming call with a fixed
    /// one-chunk text reply.
    struct FixedTextProvider {
        text: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FixedTextProvider {
        fn name(&self) -> &str {
            "fixed-text"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["fixed".to_string().into()])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
            Ok(self.text.clone())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[kod_types::ToolDefinition],
            _o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.text.clone(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>,
        > {
            let text = self.text.clone();
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::Text(text)),
                Ok(StreamChunk::Done),
            ]))
        }
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                tools: true,
                streaming_tools: true,
                ..ProviderCapabilities::conservative()
            }
        }
        /// Override the default `stream_completion` so the test does
        /// not go through the trait's collect-and-replay path — the
        /// chunks come straight from `stream`.
        fn stream_completion<'a>(
            &'a self,
            req: &'a CompletionRequest,
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>,
        > {
            let _ = req;
            self.stream("", &GenerationOptions::default())
        }
    }

    async fn engine_with_fallback(fallback_text: &str) -> (KodEngine, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = RouterConfig {
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            ..RouterConfig::default()
        };
        let engine = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();
        let mut reg = ProviderRegistry::new();
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedTextProvider {
            text: fallback_text.to_string(),
        });
        reg.insert(
            "fallback",
            provider,
            ProviderCapabilities {
                tools: true,
                streaming_tools: true,
                ..ProviderCapabilities::conservative()
            },
            "m",
        );
        engine
            .set_registry(Arc::new(reg), ModelRef::new("fallback", "m"), None)
            .await;
        (engine, tmp)
    }

    #[tokio::test]
    async fn resolvable_fallback_yields_its_stream() {
        let (engine, _tmp) = engine_with_fallback("from the fallback").await;
        let model = ModelRef::new("fallback", "m");
        let (_provider, resolved, mut stream) = engine
            .fallback_stream_for_off_track("", &[], &[], &GenerationOptions::default(), &model)
            .await
            .expect("fallback must resolve");
        assert_eq!(resolved.endpoint, "fallback");
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(StreamChunk::Text(t)) = item {
                text.push_str(&t);
            }
        }
        assert!(
            text.contains("from the fallback"),
            "expected the fallback's text, got: {text:?}",
        );
    }

    #[tokio::test]
    async fn unresolvable_fallback_returns_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = RouterConfig {
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            ..RouterConfig::default()
        };
        let engine = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();
        // Registry is empty — no provider to resolve.
        let model = ModelRef::new("nonexistent", "m");
        let r = engine
            .fallback_stream_for_off_track("", &[], &[], &GenerationOptions::default(), &model)
            .await;
        assert!(r.is_none(), "empty registry must yield None");
    }
