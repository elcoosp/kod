//! Streaming support for Ollama generation.
//!
//! Ollama uses newline-delimited JSON (NDJSON) for streaming,
//! not Server-Sent Events. Each line is a complete JSON object.

use futures::Stream;
use kod_error::{KodError, Result};
use serde::Deserialize;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Events emitted during streaming generation
///
/// Uses a flat structure: each JSON line is parsed and we distinguish
/// chunks vs done by the `done` field value.
#[derive(Debug, Clone, Deserialize)]
pub struct RawStreamEvent {
    pub model: String,
    #[serde(default)]
    pub response: String,
    pub done: bool,
    #[serde(default)]
    pub total_duration: u64,
    #[serde(default)]
    pub eval_count: i32,
    #[serde(default)]
    pub eval_duration: u64,
}

/// Events emitted during streaming generation
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of generated text
    Chunk {
        response: String,
        done: bool,
    },
    /// Final event with statistics
    Done {
        response: String,
        done: bool,
        total_duration: u64,
        eval_count: i32,
        eval_duration: u64,
    },
}

/// Accumulates streaming chunks into a complete response
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    buffer: String,
    total_duration_ns: u64,
    eval_count: i32,
    is_done: bool,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text chunk to the accumulator
    pub fn add_chunk(&mut self, text: &str) -> Result<()> {
        if self.is_done {
            return Err(KodError::Provider("Stream already finished".to_string()));
        }
        self.buffer.push_str(text);
        Ok(())
    }

    /// Mark the stream as done with final statistics
    pub fn finish_with_stats(&mut self, total_duration_ns: u64, eval_count: i32) -> Result<()> {
        self.total_duration_ns = total_duration_ns;
        self.eval_count = eval_count;
        self.is_done = true;
        Ok(())
    }

    /// Mark the stream as done (no stats)
    pub fn finish(&mut self) -> Result<String> {
        self.is_done = true;
        Ok(self.buffer.clone())
    }

    /// Get the accumulated text so far
    pub fn current_text(&self) -> &str {
        &self.buffer
    }

    /// Check if stream is done
    pub fn is_done(&self) -> bool {
        self.is_done
    }

    /// Get tokens per second if stats are available
    pub fn tokens_per_second(&self) -> Option<f64> {
        if self.total_duration_ns == 0 {
            return None;
        }
        Some(self.eval_count as f64 / (self.total_duration_ns as f64 / 1_000_000_000.0))
    }
}

/// Stream of generation events from Ollama
pub struct GenerationStream {
    receiver: mpsc::Receiver<Result<StreamEvent>>,
}

impl GenerationStream {
    pub(crate) fn new(receiver: mpsc::Receiver<Result<StreamEvent>>) -> Self {
        Self { receiver }
    }
}

impl Stream for GenerationStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

/// Parse a line of NDJSON into a StreamEvent
pub fn parse_stream_line(line: &str) -> Result<StreamEvent> {
    let raw: RawStreamEvent = serde_json::from_str(line)
        .map_err(|e| KodError::Provider(format!("Failed to parse stream line: {} - {}", line, e)))?;

    if raw.done {
        Ok(StreamEvent::Done {
            response: raw.response,
            done: raw.done,
            total_duration: raw.total_duration,
            eval_count: raw.eval_count,
            eval_duration: raw.eval_duration,
        })
    } else {
        Ok(StreamEvent::Chunk {
            response: raw.response,
            done: raw.done,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chunk() {
        let line = r#"{"model":"llama3.2","response":"Hello","done":false}"#;
        let event = parse_stream_line(line).unwrap();

        match event {
            StreamEvent::Chunk { response, done } => {
                assert_eq!(response, "Hello");
                assert!(!done);
            }
            _ => panic!("Expected chunk event"),
        }
    }

    #[test]
    fn test_parse_done() {
        let line = r#"{"model":"llama3.2","response":"","done":true,"total_duration":1000,"eval_count":10}"#;
        let event = parse_stream_line(line).unwrap();

        match event {
            StreamEvent::Done { total_duration, eval_count, .. } => {
                assert_eq!(total_duration, 1000);
                assert_eq!(eval_count, 10);
            }
            _ => panic!("Expected done event"),
        }
    }

    #[test]
    fn test_accumulator() {
        let mut acc = StreamAccumulator::new();
        acc.add_chunk("Hello").unwrap();
        acc.add_chunk(" ").unwrap();
        acc.add_chunk("world").unwrap();

        assert_eq!(acc.current_text(), "Hello world");

        let final_text = acc.finish().unwrap();
        assert_eq!(final_text, "Hello world");
        assert!(acc.is_done());
    }
}
