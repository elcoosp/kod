//! OpenAI-compatible LLM provider backed by `adk-model`.
//!
//! A single code path for every OpenAI-spec chat-completions endpoint:
//! Ollama (`http://localhost:11434/v1`), LM Studio
//! (`http://localhost:1234/v1`), MLX Omni Serve, vLLM, OpenAI itself.
//!
//! `base_url` accepts either the server root (`http://host:port`) or the
//! full API root (`http://host:port/v1`) — a missing `/v1` suffix is added
//! automatically so existing Ollama-style configs keep working.

mod provider;

pub use provider::{OpenAICompatProvider, normalize_base_url};
