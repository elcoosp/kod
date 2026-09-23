//! Soft interrupts: messages delivered to a running turn.
//!
//! The pre-P1-b shape was a `Vec<String>` of "steering notes" — every
//! producer wrote the same untyped string, and the model could not
//! tell a user instruction from a swarm conflict notice from a
//! background-task completion. jcode's design (notebook §3) types the
//! producer so the recipient can attribute the message: a user steer
//! outranks a notification, and a notice about another agent's write
//! is scoped to the agent that touched the file.
//!
//! Delivery is unchanged: [`KodEngine::apply_steers`] drains the queue
//! at each round boundary. This module only makes the *content* typed
//! and the source explicit; the transport is the same channel the TUI
//! has been using since the first steering commit.

use serde::{Deserialize, Serialize};

/// Where an interrupt came from.
///
/// The label is rendered into the message header so the model can
/// weigh the instruction correctly — "the user said this" and "another
/// agent touched a file you read" call for different responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptSource {
    /// A note typed by the user (the TUI's steering input, the CLI's
    /// `--steer` flag).
    User,
    /// A harness-level notice: compaction happened, a plan step
    /// advanced, a tool result was truncated.
    System,
    /// A background job finished or stalled (P2-d).
    BackgroundTask,
    /// Another swarm agent touched a shared file (P1-c), or the
    /// coordinator sent a note.
    Swarm,
}

impl InterruptSource {
    /// The header the model sees before the interrupt body.
    ///
    /// The `User` header is byte-identical to the pre-P1-b string so
    /// the golden-prompt tests and every recorded transcript stay
    /// valid. The others are new — a swarm notice never had a header
    /// before, and a background completion did not exist.
    pub fn header(self) -> &'static str {
        match self {
            Self::User => {
                "## User steer (new instruction — adjust course now, do not restart what already worked)"
            }
            Self::System => "## System notice (the harness changed state; read for context)",
            Self::BackgroundTask => {
                "## Background task update (a job you started has news; act only if relevant)"
            }
            Self::Swarm => {
                "## Swarm notice (another agent touched shared state; re-read before acting)"
            }
        }
    }
}

/// One queued interrupt.
#[derive(Debug, Clone)]
pub struct SoftInterrupt {
    pub content: String,
    pub source: InterruptSource,
}

impl SoftInterrupt {
    pub fn user(content: impl Into<String>) -> Self {
        Self { content: content.into(), source: InterruptSource::User }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self { content: content.into(), source: InterruptSource::System }
    }
    pub fn background(content: impl Into<String>) -> Self {
        Self { content: content.into(), source: InterruptSource::BackgroundTask }
    }
    pub fn swarm(content: impl Into<String>) -> Self {
        Self { content: content.into(), source: InterruptSource::Swarm }
    }

    /// The full text injected into the conversation.
    pub fn render(&self) -> String {
        format!("{}\n{}", self.source.header(), self.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_header_is_byte_identical_to_the_legacy_string() {
        // The pre-P1-b `apply_steers` wrote this exact header. Every
        // recorded transcript and every golden-prompt snapshot
        // depends on it; a change here is a prompt-format break, not
        // a cosmetic one.
        assert_eq!(
            InterruptSource::User.header(),
            "## User steer (new instruction — adjust course now, do not restart what already worked)",
        );
    }

    #[test]
    fn render_puts_the_header_before_the_body() {
        let i = SoftInterrupt::system("compaction ran");
        let r = i.render();
        assert!(r.starts_with("## System notice"));
        assert!(r.ends_with("compaction ran"));
    }

    #[test]
    fn each_source_has_a_distinct_header() {
        let headers = [
            InterruptSource::User.header(),
            InterruptSource::System.header(),
            InterruptSource::BackgroundTask.header(),
            InterruptSource::Swarm.header(),
        ];
        let unique: std::collections::HashSet<_> = headers.iter().collect();
        assert_eq!(unique.len(), 4, "headers must be distinguishable");
    }
}
