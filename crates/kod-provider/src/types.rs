//! Types for LLM generation.

use kod_types::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

#[derive(Debug, Clone)]
pub enum GenerationResponse {
    Text {
        content: String,
        usage: Option<TokenUsage>,
    },
    ToolCalls {
        calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
    Mixed {
        content: String,
        calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
}

impl GenerationResponse {
    pub fn usage(&self) -> Option<&TokenUsage> {
        match self {
            GenerationResponse::Text { usage, .. } => usage.as_ref(),
            GenerationResponse::ToolCalls { usage, .. } => usage.as_ref(),
            GenerationResponse::Mixed { usage, .. } => usage.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    ToolCallStart { name: String },
    ToolCallDelta { arguments: String },
    Usage(TokenUsage),
    Done,
}

/// Expand a collected [`GenerationResponse`] into the chunk sequence
/// [`LlmProvider::stream_with_tools`]'s default implementation replays.
pub fn response_chunks(response: GenerationResponse) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();
    match response {
        GenerationResponse::Text { content, .. } => {
            if !content.is_empty() {
                chunks.push(StreamChunk::Text(content));
            }
        }
        GenerationResponse::ToolCalls { calls, .. } => {
            for call in &calls {
                chunks.push(StreamChunk::ToolCallStart {
                    name: call.tool_name.clone(),
                });
                chunks.push(StreamChunk::ToolCallDelta {
                    arguments: call.arguments.to_string(),
                });
            }
        }
        GenerationResponse::Mixed { content, calls, .. } => {
            if !content.is_empty() {
                chunks.push(StreamChunk::Text(content));
            }
            for call in &calls {
                chunks.push(StreamChunk::ToolCallStart {
                    name: call.tool_name.clone(),
                });
                chunks.push(StreamChunk::ToolCallDelta {
                    arguments: call.arguments.to_string(),
                });
            }
        }
    }
    chunks.push(StreamChunk::Done);
    chunks
}
