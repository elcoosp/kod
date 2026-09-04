//! Types for LLM generation.

use kod_types::ToolCall;

#[derive(Debug, Clone)]
pub enum GenerationResponse {
    Text {
        content: String,
    },
    ToolCalls {
        calls: Vec<ToolCall>,
    },
    Mixed {
        content: String,
        calls: Vec<ToolCall>,
    },
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    ToolCallStart { name: String },
    ToolCallDelta { arguments: String },
    Done,
}
